//! Templates are used to skip diffing on any static parts of the rsx.
//! TemplateNodes are different from VNodes in that they can contain partial dynamic and static content in the same node.
//! For example:
//! ```
//! rsx! {
//!     div {
//!         color: "{color}",
//!         "Hello, world",
//!         "{dynamic_text_1}",
//!         "{dynamic_text_2}",
//!         dynamic_iterator
//!     }
//! }
//! ```
//! The above will turn into a template that contains information on how to build div { "Hello, world" } and then every refrence to the template will hydrate with the value of dynamic_text_1, dynamic_text_2, dynamic_iterator, and the color property.
//! The rsx macro will both generate the template and the `DynamicNodeMapping` struct that contains the information on what parts of the template depend on each value of the dynamic context.
//! In templates with many dynamic parts, this allows the diffing algorithm to skip traversing the template to find what part to hydrate.
//! Each dynamic part will contain a index into the dynamic context to determine what value to use. The indexes are origionally ordered by traversing the tree depth first from the root.
//! The indexes for the above would be as follows:
//! ```
//! rsx! {
//!     div {
//!         color: "{color}", // attribute index 0
//!         "Hello, world",
//!         "{dynamic_text_1}", // text index 0
//!         "{dynamic_text_2}", // text index 1
//!         dynamic_iterator // node index 0
//!     }
//! }
//! ```
//! Including these indexes allows hot reloading to move the dynamic parts of the template around.
//! The templates generated by rsx are stored as 'static refrences, but you can change the template at runtime to allow hot reloading.
//! The template could be replaced with a new one at runtime:
//! ```
//! rsx! {
//!     div {
//!         "Hello, world",
//!         dynamic_iterator // node index 0
//!         h1 {
//!             background_color: "{color}" // attribute index 0
//!             "{dynamic_text_2}", // text index 1
//!         }
//!         h1 {
//!            color: "{color}", // attribute index 0
//!            "{dynamic_text_1}", // text index 0
//!         }
//!     }
//! }
//! ```
//! Notice how the indecies are no longer in depth first traversal order, and indecies are no longer unique. Attributes and dynamic parts of the text can be duplicated, but dynamic vnodes and componets cannot.
//! To minimize the cost of allowing hot reloading on applications that do not use it there are &'static and owned versions of template nodes, and dynamic node mapping.
//!
//! Notes:
//! 1) Why does the template need to exist outside of the virtual dom?
//! The main use of the template is skipping diffing on static parts of the dom, but it is also used to make renderes more efficient. Renderers can create a template once and the clone it into place.
//! When the renderers clone the template we could those new nodes as normal vnodes, but that would interfere with the passive memory management of the nodes. This would mean that static nodes memory must be managed by the virtual dom even though those static nodes do not exist in the virtual dom.
//! 2) The template allow diffing to scale with reactivity.
//! With a virtual dom the diffing cost scales with the number of nodes in the dom. With templates the cost scales with the number of dynamic parts of the dom. The dynamic template context links any parts of the template that can change which allows the diffing algorithm to skip traversing the template and find what part to hydrate in constant time.

/// The maxiumum integer in JS
pub const JS_MAX_INT: u64 = 9007199254740991;

use fxhash::FxHashMap;
use std::{cell::Cell, hash::Hash, marker::PhantomData, ops::Index};

use bumpalo::Bump;

use crate::{
    diff::DiffState, dynamic_template_context::TemplateContext, innerlude::GlobalNodeId,
    nodes::AttributeDiscription, Attribute, AttributeValue, ElementId, Mutations,
    OwnedAttributeValue, StaticDynamicNodeMapping,
};

/// The location of a charicter. Used to track the location of rsx calls for hot reloading.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize)
)]
pub struct StaticCodeLocation {
    /// the path to the crate that contains the location
    pub crate_path: &'static str,
    /// the path within the crate to the file that contains the location
    pub file_path: &'static str,
    /// the line number of the location
    pub line: u32,
    /// the column number of the location
    pub column: u32,
}

/// The location of a charicter. Used to track the location of rsx calls for hot reloading.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(any(feature = "hot-reload", debug_assertions))]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct OwnedCodeLocation {
    /// the path to the crate that contains the location
    pub crate_path: String,
    /// the path within the crate to the file that contains the location
    pub file_path: String,
    /// the line number of the location
    pub line: u32,
    /// the column number of the location
    pub column: u32,
}

/// The location of a charicter. Used to track the location of rsx calls for hot reloading.
#[derive(Clone, Eq, Debug)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize)
)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    serde(untagged)
)]
pub enum CodeLocation {
    /// A loctation that is created at compile time.
    Static(&'static StaticCodeLocation),
    #[cfg(any(feature = "hot-reload", debug_assertions))]
    /// A loctation that is created at runtime.
    Dynamic(Box<OwnedCodeLocation>),
}

#[cfg(all(feature = "serialize", any(feature = "hot-reload", debug_assertions)))]
impl<'de> serde::Deserialize<'de> for CodeLocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::Dynamic(Box::new(OwnedCodeLocation::deserialize(
            deserializer,
        )?)))
    }
}

impl Hash for CodeLocation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            CodeLocation::Static(loc) => {
                loc.crate_path.hash(state);
                loc.file_path.hash(state);
                state.write_u32(loc.line);
                state.write_u32(loc.column);
            }
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            CodeLocation::Dynamic(loc) => {
                let (crate_path, file_path): (&str, &str) = (&loc.crate_path, &loc.file_path);
                crate_path.hash(state);
                file_path.hash(state);
                state.write_u32(loc.line);
                state.write_u32(loc.column);
            }
        }
    }
}

impl PartialEq for CodeLocation {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Static(l), Self::Static(r)) => l == r,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            (Self::Dynamic(l), Self::Dynamic(r)) => l == r,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            (Self::Static(l), Self::Dynamic(r)) => **r == **l,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            (Self::Dynamic(l), Self::Static(r)) => **l == **r,
        }
    }
}

#[cfg(any(feature = "hot-reload", debug_assertions))]
impl PartialEq<StaticCodeLocation> for OwnedCodeLocation {
    fn eq(&self, other: &StaticCodeLocation) -> bool {
        self.crate_path == other.crate_path
            && self.file_path == other.file_path
            && self.line == other.line
            && self.column == other.column
    }
}

impl CodeLocation {
    /// Get the line number of the location.
    pub fn line(&self) -> u32 {
        match self {
            CodeLocation::Static(loc) => loc.line,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            CodeLocation::Dynamic(loc) => loc.line,
        }
    }

    /// Get the column number of the location.
    pub fn column(&self) -> u32 {
        match self {
            CodeLocation::Static(loc) => loc.column,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            CodeLocation::Dynamic(loc) => loc.column,
        }
    }

    /// Get the path within the crate to the location.
    pub fn file_path(&self) -> &str {
        match self {
            CodeLocation::Static(loc) => loc.file_path,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            CodeLocation::Dynamic(loc) => loc.file_path.as_str(),
        }
    }

    /// Get the path of the crate to the location.
    pub fn crate_path(&self) -> &str {
        match self {
            CodeLocation::Static(loc) => loc.crate_path,
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            CodeLocation::Dynamic(loc) => loc.crate_path.as_str(),
        }
    }

    #[cfg(any(feature = "hot-reload", debug_assertions))]
    /// Create an owned code location from a code location.
    pub fn to_owned(&self) -> OwnedCodeLocation {
        match self {
            CodeLocation::Static(loc) => OwnedCodeLocation {
                crate_path: loc.crate_path.to_owned(),
                file_path: loc.file_path.to_owned(),
                line: loc.line,
                column: loc.column,
            },
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            CodeLocation::Dynamic(loc) => *loc.clone(),
        }
    }
}

/// get the code location of the code that called this function
#[macro_export]
macro_rules! get_line_num {
    () => {{
        const LOC: CodeLocation = CodeLocation::Static(&StaticCodeLocation {
            crate_path: env!("CARGO_MANIFEST_DIR"),
            file_path: file!(),
            line: line!(),
            column: column!(),
        });
        LOC
    }};
}

/// An Template's unique identifier within the vdom.
///
/// `TemplateId` is a refrence to the location in the code the template was created.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct TemplateId(pub CodeLocation);

/// An Template's unique identifier within the renderer.
///
/// `RendererTemplateId` is a unique id of the template sent to the renderer. It is unique across time.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct RendererTemplateId(pub usize);

impl From<RendererTemplateId> for u64 {
    fn from(id: RendererTemplateId) -> u64 {
        id.0 as u64
    }
}

/// A TemplateNode's unique identifier.
///
/// `TemplateNodeId` is a `usize` that is only unique across the template that contains it, it is not unique across multaple instances of that template.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serialize", serde(transparent))]
pub struct TemplateNodeId(pub usize);

impl From<TemplateNodeId> for u64 {
    fn from(id: TemplateNodeId) -> u64 {
        JS_MAX_INT / 2 + id.0 as u64
    }
}

/// A refrence to a template along with any context needed to hydrate it
pub struct VTemplateRef<'a> {
    pub id: Cell<Option<ElementId>>,
    pub template_id: TemplateId,
    pub dynamic_context: TemplateContext<'a>,
}

impl<'a> VTemplateRef<'a> {
    // update the template with content from the dynamic context
    pub(crate) fn hydrate<'b>(&self, template: &'b Template, diff_state: &mut DiffState<'a>) {
        fn hydrate_inner<'b, Nodes, Attributes, V, Children, Listeners, TextSegments, Text>(
            nodes: &Nodes,
            ctx: (&mut DiffState<'b>, &VTemplateRef<'b>, &Template),
        ) where
            Nodes: AsRef<[TemplateNode<Attributes, V, Children, Listeners, TextSegments, Text>]>,
            Attributes: AsRef<[TemplateAttribute<V>]>,
            V: TemplateValue,
            Children: AsRef<[TemplateNodeId]>,
            Listeners: AsRef<[usize]>,
            TextSegments: AsRef<[TextTemplateSegment<Text>]>,
            Text: AsRef<str>,
        {
            let (diff_state, template_ref, template) = ctx;
            for id in template.all_dynamic() {
                let dynamic_node = &nodes.as_ref()[id.0];
                dynamic_node.hydrate(diff_state, template_ref);
            }
        }

        template.with_nodes(hydrate_inner, hydrate_inner, (diff_state, self, template));
    }
}

/// A template that is created at compile time
#[derive(Debug, PartialEq)]
pub struct StaticTemplate {
    /// The nodes in the template
    pub nodes: StaticTemplateNodes,
    /// The ids of the root nodes in the template
    pub root_nodes: StaticRootNodes,
    /// Any nodes that contain dynamic components. This is stored in the tmeplate to avoid traversing the tree every time a template is refrenced.
    pub dynamic_mapping: StaticDynamicNodeMapping,
}

/// A template that is created at runtime
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg(any(feature = "hot-reload", debug_assertions))]
pub struct OwnedTemplate {
    /// The nodes in the template
    pub nodes: OwnedTemplateNodes,
    /// The ids of the root nodes in the template
    pub root_nodes: OwnedRootNodes,
    /// Any nodes that contain dynamic components. This is stored in the tmeplate to avoid traversing the tree every time a template is refrenced.
    pub dynamic_mapping: crate::OwnedDynamicNodeMapping,
}

/// A template used to skip diffing on some static parts of the rsx
#[derive(Debug, Clone, PartialEq)]
pub enum Template {
    /// A template that is createded at compile time
    Static(&'static StaticTemplate),
    #[cfg(any(feature = "hot-reload", debug_assertions))]
    /// A template that is created at runtime
    Owned(OwnedTemplate),
}

impl Template {
    pub(crate) fn create<'b>(
        &self,
        mutations: &mut Mutations<'b>,
        bump: &'b Bump,
        id: RendererTemplateId,
    ) {
        mutations.create_templete(id);
        let empty = match self {
            Template::Static(s) => s.nodes.is_empty(),
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            Template::Owned(o) => o.nodes.is_empty(),
        };
        let mut len = 0;
        if !empty {
            let roots = match self {
                Template::Static(s) => s.root_nodes,
                #[cfg(any(feature = "hot-reload", debug_assertions))]
                Template::Owned(o) => &o.root_nodes,
            };
            for root in roots {
                len += 1;
                self.create_node(mutations, bump, *root);
            }
        }
        mutations.finish_templete(len as u32);
    }

    fn create_node<'b>(&self, mutations: &mut Mutations<'b>, bump: &'b Bump, id: TemplateNodeId) {
        fn crate_node_inner<'b, Attributes, V, Children, Listeners, TextSegments, Text>(
            node: &TemplateNode<Attributes, V, Children, Listeners, TextSegments, Text>,
            ctx: (&mut Mutations<'b>, &'b Bump, &Template),
        ) where
            Attributes: AsRef<[TemplateAttribute<V>]>,
            V: TemplateValue,
            Children: AsRef<[TemplateNodeId]>,
            Listeners: AsRef<[usize]>,
            TextSegments: AsRef<[TextTemplateSegment<Text>]>,
            Text: AsRef<str>,
        {
            let (mutations, bump, template) = ctx;
            let id = node.id;
            let locally_static = node.locally_static;
            let fully_static = node.fully_static;
            match &node.node_type {
                TemplateNodeType::Element(el) => {
                    let TemplateElement {
                        tag,
                        namespace,
                        attributes,
                        children,
                        ..
                    } = el;
                    mutations.create_element_template(
                        tag,
                        *namespace,
                        id,
                        locally_static,
                        fully_static,
                    );
                    for attr in attributes.as_ref() {
                        if let TemplateAttributeValue::Static(val) = &attr.value {
                            let val: AttributeValue<'b> = val.allocate(bump);
                            let attribute = Attribute {
                                attribute: attr.attribute,
                                is_static: true,
                                value: val,
                            };
                            mutations.set_attribute(bump.alloc(attribute), id);
                        }
                    }
                    let mut children_created = 0;
                    for child in children.as_ref() {
                        template.create_node(mutations, bump, *child);
                        children_created += 1;
                    }

                    mutations.append_children(children_created);
                }
                TemplateNodeType::Text(text) => {
                    let mut text_iter = text.segments.as_ref().iter();
                    if let (Some(TextTemplateSegment::Static(txt)), None) =
                        (text_iter.next(), text_iter.next())
                    {
                        mutations.create_text_node_template(
                            bump.alloc_str(txt.as_ref()),
                            id,
                            locally_static,
                        );
                    } else {
                        mutations.create_text_node_template("", id, locally_static);
                    }
                }
                TemplateNodeType::DynamicNode(_) => {
                    mutations.create_placeholder_template(id);
                }
            }
        }
        self.with_node(
            id,
            crate_node_inner,
            crate_node_inner,
            (mutations, bump, self),
        );
    }

    #[cfg(any(feature = "hot-reload", debug_assertions))]
    pub(crate) fn with_node<F1, F2, Ctx, R>(
        &self,
        id: TemplateNodeId,
        mut f1: F1,
        mut f2: F2,
        ctx: Ctx,
    ) -> R
    where
        F1: FnMut(&StaticTemplateNode, Ctx) -> R,
        F2: FnMut(&OwnedTemplateNode, Ctx) -> R,
    {
        match self {
            Template::Static(s) => f1(&s.nodes[id.0], ctx),
            Template::Owned(o) => f2(&o.nodes[id.0], ctx),
        }
    }

    #[cfg(not(any(feature = "hot-reload", debug_assertions)))]
    pub(crate) fn with_node<F1, F2, Ctx, R>(
        &self,
        id: TemplateNodeId,
        mut f1: F1,
        _f2: F2,
        ctx: Ctx,
    ) -> R
    where
        F1: FnMut(&StaticTemplateNode, Ctx) -> R,
        F2: FnMut(&StaticTemplateNode, Ctx) -> R,
    {
        match self {
            Template::Static(s) => f1(&s.nodes[id.0], ctx),
        }
    }

    #[cfg(any(feature = "hot-reload", debug_assertions))]
    pub(crate) fn with_nodes<'a, F1, F2, Ctx>(&'a self, mut f1: F1, mut f2: F2, ctx: Ctx)
    where
        F1: FnMut(&'a &'static [StaticTemplateNode], Ctx),
        F2: FnMut(&'a Vec<OwnedTemplateNode>, Ctx),
    {
        match self {
            Template::Static(s) => f1(&s.nodes, ctx),
            Template::Owned(o) => f2(&o.nodes, ctx),
        }
    }

    #[cfg(not(any(feature = "hot-reload", debug_assertions)))]
    pub(crate) fn with_nodes<'a, F1, F2, Ctx>(&'a self, mut f1: F1, _f2: F2, ctx: Ctx)
    where
        F1: FnMut(&'a &'static [StaticTemplateNode], Ctx),
        F2: FnMut(&'a &'static [StaticTemplateNode], Ctx),
    {
        match self {
            Template::Static(s) => f1(&s.nodes, ctx),
        }
    }

    pub(crate) fn all_dynamic<'a>(&'a self) -> Box<dyn Iterator<Item = TemplateNodeId> + 'a> {
        match self {
            Template::Static(s) => Box::new(s.dynamic_mapping.all_dynamic()),
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            Template::Owned(o) => Box::new(o.dynamic_mapping.all_dynamic()),
        }
    }

    pub(crate) fn volatile_attributes<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = (TemplateNodeId, usize)> + 'a> {
        match self {
            Template::Static(s) => Box::new(
                s.dynamic_mapping
                    .volatile_attributes
                    .as_ref()
                    .iter()
                    .copied(),
            ),
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            Template::Owned(o) => Box::new(o.dynamic_mapping.volatile_attributes.iter().copied()),
        }
    }

    pub(crate) fn get_dynamic_nodes_for_node_index(&self, idx: usize) -> Option<TemplateNodeId> {
        match self {
            Template::Static(s) => s.dynamic_mapping.nodes[idx],
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            Template::Owned(o) => o.dynamic_mapping.nodes[idx],
        }
    }

    pub(crate) fn get_dynamic_nodes_for_text_index(&self, idx: usize) -> &[TemplateNodeId] {
        match self {
            Template::Static(s) => s.dynamic_mapping.text[idx],
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            Template::Owned(o) => o.dynamic_mapping.text[idx].as_ref(),
        }
    }

    pub(crate) fn get_dynamic_nodes_for_attribute_index(
        &self,
        idx: usize,
    ) -> &[(TemplateNodeId, usize)] {
        match self {
            Template::Static(s) => s.dynamic_mapping.attributes[idx],
            #[cfg(any(feature = "hot-reload", debug_assertions))]
            Template::Owned(o) => o.dynamic_mapping.attributes[idx].as_ref(),
        }
    }
}

/// A array of stack allocated Template nodes
pub type StaticTemplateNodes = &'static [StaticTemplateNode];
#[cfg(any(feature = "hot-reload", debug_assertions))]
/// A vec of heep allocated Template nodes
pub type OwnedTemplateNodes = Vec<OwnedTemplateNode>;

/// A stack allocated Template node
pub type StaticTemplateNode = TemplateNode<
    &'static [TemplateAttribute<StaticAttributeValue>],
    StaticAttributeValue,
    &'static [TemplateNodeId],
    &'static [usize],
    &'static [TextTemplateSegment<&'static str>],
    &'static str,
>;

#[cfg(any(feature = "hot-reload", debug_assertions))]
/// A heap allocated Template node
pub type OwnedTemplateNode = TemplateNode<
    Vec<TemplateAttribute<OwnedAttributeValue>>,
    OwnedAttributeValue,
    Vec<TemplateNodeId>,
    Vec<usize>,
    Vec<TextTemplateSegment<String>>,
    String,
>;

/// A stack allocated list of root Template nodes
pub type StaticRootNodes = &'static [TemplateNodeId];

#[cfg(any(feature = "hot-reload", debug_assertions))]
/// A heap allocated list of root Template nodes
pub type OwnedRootNodes = Vec<TemplateNodeId>;

/// Templates can only contain a limited subset of VNodes and keys are not needed, as diffing will be skipped.
/// Dynamic parts of the Template are inserted into the VNode using the `TemplateContext` by traversing the tree in order and filling in dynamic parts
/// This template node is generic over the storage of the nodes to allow for owned and &'static versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct TemplateNode<Attributes, V, Children, Listeners, TextSegments, Text>
where
    Attributes: AsRef<[TemplateAttribute<V>]>,
    V: TemplateValue,
    Children: AsRef<[TemplateNodeId]>,
    Listeners: AsRef<[usize]>,
    TextSegments: AsRef<[TextTemplateSegment<Text>]>,
    Text: AsRef<str>,
{
    /// The ID of the [`TemplateNode`]. Note that this is not an elenemt id, and should be allocated seperately from VNodes on the frontend.
    pub id: TemplateNodeId,
    /// If the id of the node must be kept in the refrences
    pub locally_static: bool,
    /// If any children of this node must be kept in the references
    pub fully_static: bool,
    /// The type of the [`TemplateNode`].
    pub node_type: TemplateNodeType<Attributes, V, Children, Listeners, TextSegments, Text>,
}

impl<Attributes, V, Children, Listeners, TextSegments, Text>
    TemplateNode<Attributes, V, Children, Listeners, TextSegments, Text>
where
    Attributes: AsRef<[TemplateAttribute<V>]>,
    V: TemplateValue,
    Children: AsRef<[TemplateNodeId]>,
    Listeners: AsRef<[usize]>,
    TextSegments: AsRef<[TextTemplateSegment<Text>]>,
    Text: AsRef<str>,
{
    fn hydrate<'b>(&self, diff_state: &mut DiffState<'b>, template_ref: &VTemplateRef<'b>) {
        let real_id = template_ref.id.get().unwrap();

        diff_state.element_stack.push(GlobalNodeId::TemplateId {
            template_ref_id: real_id,
            template_node_id: self.id,
        });
        diff_state.mutations.enter_template_ref(real_id);
        match &self.node_type {
            TemplateNodeType::Element(el) => {
                let TemplateElement {
                    attributes,
                    listeners,
                    ..
                } = el;
                for attr in attributes.as_ref() {
                    if let TemplateAttributeValue::Dynamic(idx) = attr.value {
                        let attribute = Attribute {
                            attribute: attr.attribute,
                            value: template_ref
                                .dynamic_context
                                .resolve_attribute(idx)
                                .to_owned(),
                            is_static: false,
                        };
                        let scope_bump = diff_state.current_scope_bump();
                        diff_state
                            .mutations
                            .set_attribute(scope_bump.alloc(attribute), self.id);
                    }
                }
                for listener_idx in listeners.as_ref() {
                    let listener = template_ref.dynamic_context.resolve_listener(*listener_idx);
                    let global_id = GlobalNodeId::TemplateId {
                        template_ref_id: real_id,
                        template_node_id: self.id,
                    };
                    listener.mounted_node.set(Some(global_id));
                    diff_state
                        .mutations
                        .new_event_listener(listener, diff_state.current_scope());
                }
            }
            TemplateNodeType::Text(text) => {
                let new_text = template_ref
                    .dynamic_context
                    .resolve_text(&text.segments.as_ref());
                let scope_bump = diff_state.current_scope_bump();
                diff_state
                    .mutations
                    .set_text(scope_bump.alloc(new_text), self.id)
            }
            TemplateNodeType::DynamicNode(idx) => {
                // this will only be triggered for root elements
                let created =
                    diff_state.create_node(template_ref.dynamic_context.resolve_node(*idx));
                diff_state.mutations.replace_with(self.id, created as u32);
            }
        }
        diff_state.mutations.exit_template_ref();
        diff_state.element_stack.pop();
    }
}

/// A template for an attribute
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct TemplateAttribute<V: TemplateValue> {
    /// The discription of the attribute
    pub attribute: AttributeDiscription,
    /// The value of the attribute
    pub value: TemplateAttributeValue<V>,
}

/// A template attribute value that is either dynamic or static
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub enum TemplateAttributeValue<V: TemplateValue> {
    /// A static attribute
    Static(V),
    /// A dynamic attribute
    Dynamic(usize),
}

/// The value for an attribute in a template
pub trait TemplateValue {
    /// Allocates the attribute in a bump allocator
    fn allocate<'b>(&self, bump: &'b Bump) -> AttributeValue<'b>;
}

impl TemplateValue for StaticAttributeValue {
    fn allocate<'b>(&self, bump: &'b Bump) -> AttributeValue<'b> {
        match self.clone() {
            StaticAttributeValue::Text(txt) => AttributeValue::Text(bump.alloc_str(txt)),
            StaticAttributeValue::Bytes(bytes) => {
                AttributeValue::Bytes(bump.alloc_slice_copy(bytes))
            }
            StaticAttributeValue::Float32(f) => AttributeValue::Float32(f),
            StaticAttributeValue::Float64(f) => AttributeValue::Float64(f),
            StaticAttributeValue::Int32(i) => AttributeValue::Int32(i),
            StaticAttributeValue::Int64(i) => AttributeValue::Int64(i),
            StaticAttributeValue::Uint32(u) => AttributeValue::Uint32(u),
            StaticAttributeValue::Uint64(u) => AttributeValue::Uint64(u),
            StaticAttributeValue::Bool(b) => AttributeValue::Bool(b),
            StaticAttributeValue::Vec3Float(f1, f2, f3) => AttributeValue::Vec3Float(f1, f2, f3),
            StaticAttributeValue::Vec3Int(i1, i2, i3) => AttributeValue::Vec3Int(i1, i2, i3),
            StaticAttributeValue::Vec3Uint(u1, u2, u3) => AttributeValue::Vec3Uint(u1, u2, u3),
            StaticAttributeValue::Vec4Float(f1, f2, f3, f4) => {
                AttributeValue::Vec4Float(f1, f2, f3, f4)
            }
            StaticAttributeValue::Vec4Int(i1, i2, i3, i4) => {
                AttributeValue::Vec4Int(i1, i2, i3, i4)
            }
            StaticAttributeValue::Vec4Uint(u1, u2, u3, u4) => {
                AttributeValue::Vec4Uint(u1, u2, u3, u4)
            }
        }
    }
}

#[cfg(any(feature = "hot-reload", debug_assertions))]
impl TemplateValue for OwnedAttributeValue {
    fn allocate<'b>(&self, bump: &'b Bump) -> AttributeValue<'b> {
        match self.clone() {
            OwnedAttributeValue::Text(txt) => AttributeValue::Text(bump.alloc(txt)),
            OwnedAttributeValue::Bytes(bytes) => AttributeValue::Bytes(bump.alloc(bytes)),
            OwnedAttributeValue::Float32(f) => AttributeValue::Float32(f),
            OwnedAttributeValue::Float64(f) => AttributeValue::Float64(f),
            OwnedAttributeValue::Int32(i) => AttributeValue::Int32(i),
            OwnedAttributeValue::Int64(i) => AttributeValue::Int64(i),
            OwnedAttributeValue::Uint32(u) => AttributeValue::Uint32(u),
            OwnedAttributeValue::Uint64(u) => AttributeValue::Uint64(u),
            OwnedAttributeValue::Bool(b) => AttributeValue::Bool(b),
            OwnedAttributeValue::Vec3Float(f1, f2, f3) => AttributeValue::Vec3Float(f1, f2, f3),
            OwnedAttributeValue::Vec3Int(i1, i2, i3) => AttributeValue::Vec3Int(i1, i2, i3),
            OwnedAttributeValue::Vec3Uint(u1, u2, u3) => AttributeValue::Vec3Uint(u1, u2, u3),
            OwnedAttributeValue::Vec4Float(f1, f2, f3, f4) => {
                AttributeValue::Vec4Float(f1, f2, f3, f4)
            }
            OwnedAttributeValue::Vec4Int(i1, i2, i3, i4) => AttributeValue::Vec4Int(i1, i2, i3, i4),
            OwnedAttributeValue::Vec4Uint(u1, u2, u3, u4) => {
                AttributeValue::Vec4Uint(u1, u2, u3, u4)
            }
            OwnedAttributeValue::Any(owned) => {
                AttributeValue::Any(crate::ArbitraryAttributeValue {
                    value: bump.alloc(owned.value),
                    cmp: owned.cmp,
                })
            }
        }
    }
}

/// The kind of node the template is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub enum TemplateNodeType<Attributes, V, Children, Listeners, TextSegments, Text>
where
    Attributes: AsRef<[TemplateAttribute<V>]>,
    Children: AsRef<[TemplateNodeId]>,
    Listeners: AsRef<[usize]>,
    V: TemplateValue,
    TextSegments: AsRef<[TextTemplateSegment<Text>]>,
    Text: AsRef<str>,
{
    /// A element node (e.g. div{}).
    Element(TemplateElement<Attributes, V, Children, Listeners>),
    /// A text node (e.g. "Hello World").
    Text(TextTemplate<TextSegments, Text>),
    /// A dynamic node (e.g. (0..10).map(|i| cx.render(rsx!{div{}})))
    /// The index in the dynamic node array this node should be replaced with
    DynamicNode(usize),
}

impl<Attributes, V, Children, Listeners, TextSegments, Text>
    TemplateNodeType<Attributes, V, Children, Listeners, TextSegments, Text>
where
    Attributes: AsRef<[TemplateAttribute<V>]>,
    Children: AsRef<[TemplateNodeId]>,
    Listeners: AsRef<[usize]>,
    V: TemplateValue,
    TextSegments: AsRef<[TextTemplateSegment<Text>]>,
    Text: AsRef<str>,
{
    /// Returns if this node, and its children, are static.
    pub fn fully_static<Nodes: Index<usize, Output = Self>>(&self, nodes: &Nodes) -> bool {
        self.locally_static()
            && match self {
                TemplateNodeType::Element(e) => e
                    .children
                    .as_ref()
                    .iter()
                    .all(|c| nodes[c.0].fully_static(nodes)),
                TemplateNodeType::Text(_) => true,
                TemplateNodeType::DynamicNode(_) => unreachable!(),
            }
    }

    /// Returns if this node is static.
    pub fn locally_static(&self) -> bool {
        match self {
            TemplateNodeType::Element(e) => {
                e.attributes.as_ref().iter().all(|a| match a.value {
                    TemplateAttributeValue::Static(_) => true,
                    TemplateAttributeValue::Dynamic(_) => false,
                }) && e.listeners.as_ref().is_empty()
            }
            TemplateNodeType::Text(t) => t.segments.as_ref().iter().all(|seg| match seg {
                TextTemplateSegment::Static(_) => true,
                TextTemplateSegment::Dynamic(_) => false,
            }),
            TemplateNodeType::DynamicNode(_) => false,
        }
    }
}

type StaticStr = &'static str;

/// A element template
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct TemplateElement<Attributes, V, Children, Listeners>
where
    Attributes: AsRef<[TemplateAttribute<V>]>,
    Children: AsRef<[TemplateNodeId]>,
    Listeners: AsRef<[usize]>,
    V: TemplateValue,
{
    /// The tag name of the element
    #[cfg_attr(
        all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
        serde(deserialize_with = "crate::util::deserialize_static_leaky")
    )]
    pub tag: StaticStr,
    /// The namespace of the element
    #[cfg_attr(
        all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
        serde(deserialize_with = "crate::util::deserialize_static_leaky_ns")
    )]
    pub namespace: Option<StaticStr>,
    /// The attributes that modify the element
    pub attributes: Attributes,
    /// The ids of the children of the element
    pub children: Children,
    /// The ids of the listeners of the element
    pub listeners: Listeners,
    /// The parent of the element
    pub parent: Option<TemplateNodeId>,
    value: PhantomData<V>,
}

impl<Attributes, V, Children, Listeners> TemplateElement<Attributes, V, Children, Listeners>
where
    Attributes: AsRef<[TemplateAttribute<V>]>,
    Children: AsRef<[TemplateNodeId]>,
    Listeners: AsRef<[usize]>,
    V: TemplateValue,
{
    /// create a new element template
    pub const fn new(
        tag: &'static str,
        namespace: Option<&'static str>,
        attributes: Attributes,
        children: Children,
        listeners: Listeners,
        parent: Option<TemplateNodeId>,
    ) -> Self {
        TemplateElement {
            tag,
            namespace,
            attributes,
            children,
            listeners,
            parent,
            value: PhantomData,
        }
    }
}

/// A template for some text that may contain dynamic segments for example "Hello {name}" contains the static segment "Hello " and the dynamic segment "{name}".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub struct TextTemplate<Segments, Text>
where
    Segments: AsRef<[TextTemplateSegment<Text>]>,
    Text: AsRef<str>,
{
    /// The segments of the template.
    pub segments: Segments,
    text: PhantomData<Text>,
}

impl<Segments, Text> TextTemplate<Segments, Text>
where
    Segments: AsRef<[TextTemplateSegment<Text>]>,
    Text: AsRef<str>,
{
    /// create a new template from the segments it is composed of.
    pub const fn new(segments: Segments) -> Self {
        TextTemplate {
            segments,
            text: PhantomData,
        }
    }
}

/// A segment of a text template that may be dynamic or static.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    all(feature = "serialize", any(feature = "hot-reload", debug_assertions)),
    derive(serde::Serialize, serde::Deserialize)
)]
pub enum TextTemplateSegment<Text>
where
    Text: AsRef<str>,
{
    /// A constant text segment
    Static(Text),
    /// A dynamic text segment
    Dynamic(usize),
}

/// A template value that is created at compile time that is sync.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)]
pub enum StaticAttributeValue {
    Text(&'static str),
    Float32(f32),
    Float64(f64),
    Int32(i32),
    Int64(i64),
    Uint32(u32),
    Uint64(u64),
    Bool(bool),

    Vec3Float(f32, f32, f32),
    Vec3Int(i32, i32, i32),
    Vec3Uint(u32, u32, u32),

    Vec4Float(f32, f32, f32, f32),
    Vec4Int(i32, i32, i32, i32),
    Vec4Uint(u32, u32, u32, u32),

    Bytes(&'static [u8]),
}

#[derive(Default)]
pub(crate) struct TemplateResolver {
    // maps a id to the rendererid and if that template needs to be re-created
    pub template_id_mapping: FxHashMap<TemplateId, (RendererTemplateId, bool)>,
    pub template_count: usize,
}

impl TemplateResolver {
    #[cfg(any(feature = "hot-reload", debug_assertions))]
    pub fn mark_dirty(&mut self, id: &TemplateId) {
        if let Some((_, dirty)) = self.template_id_mapping.get_mut(id) {
            println!("marking dirty {:?}", id);
            *dirty = true;
        } else {
            println!("failed {:?}", id);
        }
    }

    pub fn is_dirty(&self, id: &TemplateId) -> bool {
        matches!(self.template_id_mapping.get(id), Some((_, true)))
    }

    // returns (id, if the id was created)
    pub fn get_or_create_client_id(
        &mut self,
        template_id: &TemplateId,
    ) -> (RendererTemplateId, bool) {
        if let Some(id) = self.template_id_mapping.get(template_id) {
            *id
        } else {
            let id = self.template_count;
            let renderer_id = RendererTemplateId(id);
            self.template_id_mapping
                .insert(template_id.clone(), (renderer_id, false));
            self.template_count += 1;
            (renderer_id, true)
        }
    }
}

#[cfg(any(feature = "hot-reload", debug_assertions))]
/// A message telling the virtual dom to set a template
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serialize", derive(serde::Serialize, serde::Deserialize))]
pub struct SetTemplateMsg(pub TemplateId, pub OwnedTemplate);