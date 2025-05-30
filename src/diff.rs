//! Diffs two trees and runs a set of callbacks for each difference.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use anyhow::Result;
use camino::Utf8Path;

use crate::hashing::ObjectId;
use crate::tree::{self, Forest, Node, NodeType, Tree};

pub trait Callbacks {
    /// A tree node with the given path was added
    fn node_added(&mut self, node_path: &Utf8Path, new_node: &Node, forest: &Forest) -> Result<()>;

    /// A tree node at the given path was removed
    fn node_removed(
        &mut self,
        node_path: &Utf8Path,
        old_node: &Node,
        forest: &Forest,
    ) -> Result<()>;

    /// The contents of a file or symlink changed (not called on directories).
    /// Presume metadata also changed.
    fn contents_changed(
        &mut self,
        node_path: &Utf8Path,
        old_node: &Node,
        new_node: &Node,
    ) -> Result<()>;

    /// A node's metadata was changed.
    fn metadata_changed(
        &mut self,
        node_path: &Utf8Path,
        old_node: &Node,
        new_node: &Node,
    ) -> Result<()>;

    /// A node didn't change.
    fn nothing_changed(&mut self, _node_path: &Utf8Path, _node: &Node) -> Result<()> {
        Ok(())
    }

    /// Called when the type of a node changed.
    ///
    /// For most cases this can be modeled as removing the old node
    /// and adding the new node, so make that the default behavior.
    fn type_changed(
        &mut self,
        node_path: &Utf8Path,
        old_node: &Node,
        old_forest: &Forest,
        new_node: &Node,
        new_forest: &Forest,
    ) -> Result<()> {
        self.node_removed(node_path, old_node, old_forest)?;
        self.node_added(node_path, new_node, new_forest)?;
        Ok(())
    }
}

/// Provide an empty forest and a ID to the empty tree.
/// Useful for comparisons to nothing (e.g., the first snapshot's diff)
pub fn null_forest() -> &'static (ObjectId, Forest) {
    static NF: LazyLock<(ObjectId, Forest)> = LazyLock::new(|| {
        let empty_tree = Tree::new();
        let (_, eid) = tree::serialize_and_hash(&empty_tree).unwrap();
        let mut empty_forest = Forest::default();
        empty_forest.insert(eid, std::sync::Arc::new(empty_tree));
        (eid, empty_forest)
    });
    &NF
}

pub fn compare_trees(
    (id1, forest1): (&ObjectId, &Forest),
    (id2, forest2): (&ObjectId, &Forest),
    tree_path: &Utf8Path,
    callbacks: &mut dyn Callbacks,
) -> Result<()> {
    let tree1: &Tree = forest1
        .get(id1)
        .unwrap_or_else(|| panic!("Missing tree {}", id1));

    let tree2: &Tree = forest2
        .get(id2)
        .unwrap_or_else(|| panic!("Missing tree {}", id2));

    let all_paths = tree1.keys().chain(tree2.keys()).collect::<BTreeSet<_>>();
    for path in all_paths {
        let mut node_path = tree_path.to_owned();
        node_path.push(path);
        match (tree1.get(path), tree2.get(path)) {
            (None, None) => unreachable!(),
            (None, Some(new_node)) => callbacks.node_added(&node_path, new_node, forest2),
            (Some(old_node), None) => callbacks.node_removed(&node_path, old_node, forest1),
            (Some(l), Some(r)) => compare_nodes((l, forest1), (r, forest2), &node_path, callbacks),
        }?;
    }
    Ok(())
}

pub fn compare_nodes(
    (node1, forest1): (&Node, &Forest),
    (node2, forest2): (&Node, &Forest),
    path: &Utf8Path,
    callbacks: &mut dyn Callbacks,
) -> Result<()> {
    match (node1.kind(), node2.kind()) {
        (NodeType::File, NodeType::File) | (NodeType::Symlink, NodeType::Symlink) => {
            if node1.contents != node2.contents {
                callbacks.contents_changed(path, node1, node2)
            } else if node1.metadata != node2.metadata {
                // trace!("{:#?} != {:#?}", node1.metadata, node2.metadata);
                callbacks.metadata_changed(path, node1, node2)
            } else {
                callbacks.nothing_changed(path, node2)
            }
        }
        (NodeType::Directory, NodeType::Directory) => {
            let mut changed = false;
            // Both are directories
            if node1.contents != node2.contents {
                compare_trees(
                    (node1.contents.subtree(), forest1),
                    (node2.contents.subtree(), forest2),
                    path,
                    callbacks,
                )?;
                changed = true;
            }
            if node1.metadata != node2.metadata {
                // trace!("{:#?} != {:#?}", node1.metadata, node2.metadata);
                callbacks.metadata_changed(path, node1, node2)?;
                changed = true;
            }
            if !changed {
                callbacks.nothing_changed(path, node1)?;
            }
            Ok(())
        }
        _ => callbacks.type_changed(path, node1, forest1, node2, forest2),
    }
}
