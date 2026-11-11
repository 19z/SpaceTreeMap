use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

use mft::MftReference;

pub type NodeId = usize;
pub const ROOT: NodeId = 0;

#[derive(Clone, Debug)]
pub struct LinkName {
    pub parent: MftReference,
    pub name: OsString,
    pub dos: bool,
}

#[derive(Default, Debug)]
pub struct FileRecord {
    pub sequence: u16,
    pub is_dir: bool,
    pub names: Vec<LinkName>,
    /// Only the unnamed $DATA stream; read from DATA headers, never FILE_NAME's stale size.
    pub logical_size: u64,
}

#[derive(Debug)]
pub struct Node {
    pub record: u64,
    pub name: OsString,
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub is_dir: bool,
    pub size: u64,
    pub valid_path: bool,
}

#[derive(Default, Debug)]
pub struct Statistics {
    pub records_read: u64,
    pub files: usize,
    pub directories: usize,
    pub skipped_records: usize,
    pub orphaned: usize,
}

#[derive(Debug)]
pub struct Index {
    pub volume: PathBuf,
    pub nodes: Vec<Node>,
    pub stats: Statistics,
}

impl Index {
    pub fn build(
        volume: PathBuf,
        mut records: HashMap<u64, FileRecord>,
        mut stats: Statistics,
        cancel: &AtomicBool,
    ) -> Result<Self, String> {
        if cancel.load(Ordering::Relaxed) {
            return Err("扫描已取消".into());
        }
        let root_sequence = records
            .remove(&5)
            .ok_or("MFT 中没有有效的根目录记录")?
            .sequence;
        let mut nodes = vec![Node {
            record: 5,
            name: volume.as_os_str().to_owned(),
            parent: None,
            children: Vec::new(),
            is_dir: true,
            size: 0,
            valid_path: true,
        }];
        let mut directories = HashMap::from([(5, (ROOT, root_sequence))]);
        let mut pending = Vec::new();
        for (record_id, mut file) in records {
            if cancel.load(Ordering::Relaxed) {
                return Err("扫描已取消".into());
            }
            // A DOS alias is the same path as its long name, not another hard link.
            let long_parents: Vec<_> = file
                .names
                .iter()
                .filter(|name| !name.dos)
                .map(|name| name.parent)
                .collect();
            file.names
                .retain(|name| !name.dos || !long_parents.contains(&name.parent));
            file.names.sort_by(|a, b| a.name.cmp(&b.name));
            file.names
                .dedup_by(|a, b| a.parent == b.parent && a.name == b.name);
            if file.is_dir {
                file.names.truncate(1);
            }
            for link in file.names {
                if link.name.is_empty() || link.name == "." || link.name == ".." {
                    continue;
                }
                let id = nodes.len();
                if file.is_dir {
                    directories.insert(record_id, (id, file.sequence));
                    stats.directories += 1;
                } else {
                    stats.files += 1;
                }
                pending.push((id, link.parent));
                nodes.push(Node {
                    record: record_id,
                    name: link.name,
                    parent: None,
                    children: Vec::new(),
                    is_dir: file.is_dir,
                    size: if file.is_dir { 0 } else { file.logical_size },
                    valid_path: false,
                });
            }
        }
        let orphan = nodes.len();
        nodes.push(Node {
            record: u64::MAX,
            name: "[无法还原路径]".into(),
            parent: Some(ROOT),
            children: Vec::new(),
            is_dir: true,
            size: 0,
            valid_path: false,
        });
        for (id, reference) in pending {
            let parent = directories
                .get(&reference.entry)
                .filter(|(_, sequence)| *sequence == reference.sequence)
                .map(|(parent, _)| *parent)
                .unwrap_or(orphan);
            nodes[id].parent = Some(parent);
        }
        // Break malformed/cyclic parent chains iteratively; deep directories cannot overflow the stack.
        let mut states = vec![0u8; nodes.len()];
        states[ROOT] = 2;
        states[orphan] = 2;
        for start in 1..orphan {
            if cancel.load(Ordering::Relaxed) {
                return Err("扫描已取消".into());
            }
            let mut cursor = start;
            let mut chain = Vec::new();
            while states[cursor] == 0 {
                states[cursor] = 1;
                chain.push(cursor);
                cursor = nodes[cursor].parent.unwrap_or(orphan);
            }
            if states[cursor] == 1 {
                nodes[cursor].parent = Some(orphan);
            }
            for id in chain {
                states[id] = 2;
            }
        }
        for id in 1..nodes.len() {
            let parent = nodes[id].parent.unwrap_or(orphan);
            nodes[parent].children.push(id);
        }
        let mut traversal = VecDeque::from([ROOT]);
        while let Some(parent) = traversal.pop_front() {
            let valid = nodes[parent].valid_path;
            let children = nodes[parent].children.clone();
            for id in children {
                nodes[id].valid_path = valid && id != orphan;
                if !nodes[id].valid_path {
                    stats.orphaned += usize::from(id != orphan);
                }
                traversal.push_back(id);
            }
        }
        let mut remaining: Vec<usize> = nodes.iter().map(|node| node.children.len()).collect();
        let mut leaves: VecDeque<usize> = remaining
            .iter()
            .enumerate()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| id)
            .collect();
        while let Some(id) = leaves.pop_front() {
            if let Some(parent) = nodes[id].parent {
                nodes[parent].size = nodes[parent].size.saturating_add(nodes[id].size);
                remaining[parent] -= 1;
                if remaining[parent] == 0 {
                    leaves.push_back(parent);
                }
            }
        }
        if nodes[orphan].children.is_empty() {
            nodes[ROOT].children.retain(|id| *id != orphan);
        }
        // Sort child IDs without cloning a file tree on every UI frame.
        for id in 0..nodes.len() {
            let mut children = std::mem::take(&mut nodes[id].children);
            children.sort_unstable_by(|a, b| {
                nodes[*b]
                    .size
                    .cmp(&nodes[*a].size)
                    .then_with(|| nodes[*a].name.cmp(&nodes[*b].name))
            });
            nodes[id].children = children;
        }
        Ok(Self {
            volume,
            nodes,
            stats,
        })
    }

    pub fn path(&self, id: NodeId) -> Option<PathBuf> {
        if !self.nodes.get(id)?.valid_path {
            return None;
        }
        let mut components = Vec::new();
        let mut cursor = id;
        while cursor != ROOT {
            components.push(&self.nodes[cursor].name);
            cursor = self.nodes[cursor].parent?;
        }
        let mut path = self.volume.clone();
        for component in components.into_iter().rev() {
            path.push(component);
        }
        Some(path)
    }

    pub fn breadcrumbs(&self, mut id: NodeId) -> Vec<NodeId> {
        let mut ids = vec![id];
        while let Some(parent) = self.nodes[id].parent {
            ids.push(parent);
            id = parent;
        }
        ids.reverse();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(
        sequence: u16,
        dir: bool,
        parent: u64,
        parent_sequence: u16,
        name: &str,
        size: u64,
    ) -> FileRecord {
        FileRecord {
            sequence,
            is_dir: dir,
            names: vec![LinkName {
                parent: MftReference::new(parent, parent_sequence),
                name: name.into(),
                dos: false,
            }],
            logical_size: size,
        }
    }
    #[test]
    fn unicode_hard_links_and_dos_alias_are_aggregated_by_path() {
        let mut file = record(1, false, 10, 2, "中文文件.txt", 123);
        file.names.push(LinkName {
            parent: MftReference::new(5, 1),
            name: "link.txt".into(),
            dos: false,
        });
        file.names.push(LinkName {
            parent: MftReference::new(10, 2),
            name: "ZHONGW~1.TXT".into(),
            dos: true,
        });
        let index = Index::build(
            "C:\\".into(),
            HashMap::from([
                (5, record(1, true, 5, 1, ".", 0)),
                (10, record(2, true, 5, 1, "目录", 0)),
                (11, file),
            ]),
            Statistics::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(index.nodes[ROOT].size, 246);
        assert_eq!(index.stats.files, 2);
        assert!(index
            .nodes
            .iter()
            .enumerate()
            .any(|(id, _)| index.path(id) == Some(PathBuf::from("C:\\目录\\中文文件.txt"))));
    }
    #[test]
    fn stale_parent_references_and_cycles_have_no_shell_path() {
        let index = Index::build(
            "C:\\".into(),
            HashMap::from([
                (5, record(1, true, 5, 1, ".", 0)),
                (10, record(1, true, 11, 1, "cycle1", 0)),
                (11, record(1, true, 10, 1, "cycle2", 0)),
                (12, record(1, false, 5, 99, "stale", 20)),
            ]),
            Statistics::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(index.nodes[ROOT].size, 20);
        assert_eq!(index.stats.orphaned, 3);
        for (id, node) in index.nodes.iter().enumerate().skip(1) {
            assert!(!node.valid_path);
            assert!(index.path(id).is_none());
        }
    }
    #[test]
    fn empty_root_and_pre_cancelled_build() {
        let records = || HashMap::from([(5, record(1, true, 5, 1, ".", 0))]);
        let index = Index::build(
            "C:\\".into(),
            records(),
            Statistics::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(index.nodes[0].size, 0);
        assert!(index.nodes[0].children.is_empty());
        assert_eq!(
            Index::build(
                "C:\\".into(),
                records(),
                Statistics::default(),
                &AtomicBool::new(true)
            )
            .unwrap_err(),
            "扫描已取消"
        );
    }
}
