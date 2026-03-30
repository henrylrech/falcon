use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

/// All messages go through a single channel, preserving strict ordering.
pub enum Msg {
    DirCount { },
    /// Dir or file entry. Dirs have size=0 until SizeReady arrives.
    Entry { parent: PathBuf, node: Node },
    /// Size computed for a dir — always arrives right after its Entry.
    SizeReady { path: PathBuf, size: u64 },
    Done,
}

pub fn format_size(bytes: u64) -> String {
    let kb = 1024.0_f64;
    let mb = kb * 1024.0;
    let gb = mb * 1024.0;
    let tb = gb * 1024.0;
    let b = bytes as f64;
    if b >= tb { format!("{:.2} TB", b / tb) }
    else if b >= gb { format!("{:.2} GB", b / gb) }
    else if b >= mb { format!("{:.2} MB", b / mb) }
    else if b >= kb { format!("{:.2} KB", b / kb) }
    else { format!("{} B", bytes) }
}

pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Ok(meta) = fs::metadata(&p) {
                if meta.is_dir() { 
                    total += dir_size(&p);
                }
                else { 
                    total += meta.len(); 
                }
            }
        }
    }
    total
}

/// Scans `root` up to `max_depth` levels deep through a single ordered channel.
///
/// For each directory:
///   1. Sends Msg::Entry (size=0)   → UI shows ⏳
///   2. Computes dir_size (blocks)
///   3. Sends Msg::SizeReady        → UI updates size or removes if 0
///   4. Recurses into subdirs
///
/// The UI processes one message per frame in arrival order, so the sequence
/// Entry → SizeReady is always respected.
pub fn scan_depth(
    root: PathBuf,
    max_depth: usize,
    tx: Sender<Msg>,
    ctx_repaint: impl Fn() + Send + Sync + 'static,
) {
    let repaint = std::sync::Arc::new(ctx_repaint);
    scan_inner(&root, 0, max_depth, &tx, &repaint);
    let _ = tx.send(Msg::Done);
    repaint();
}

fn scan_inner(
    path: &Path,
    depth: usize,
    max_depth: usize,
    tx: &Sender<Msg>,
    repaint: &std::sync::Arc<impl Fn() + Send + Sync + 'static>,
) {
    if depth >= max_depth { return; }

    let entries: Vec<_> = match fs::read_dir(path) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => return,
    };

    let _ = tx.send(Msg::DirCount { });

    for entry in entries {
        let p = entry.path();
        let meta = match fs::metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let is_dir = meta.is_dir();
        let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();

        if is_dir {
            // Send entry with size=0 first so UI can show ⏳ immediately
            let node = Node { name, path: p.clone(), size: 0, is_dir: true };
            if tx.send(Msg::Entry { parent: path.to_path_buf(), node }).is_err() { return; }
            repaint();

            // Only after size is resolved do we recurse (and send more entries)
            scan_inner(&p, depth + 1, max_depth, tx, repaint);
            repaint();

            // Block computing size — no other message is sent until this is done
            let size = dir_size(&p);

            // SizeReady always follows its Entry in the same channel
            if tx.send(Msg::SizeReady { path: p.clone(), size }).is_err() { return; }
            repaint();
            
        } else {
            let size = meta.len();
            if size == 0 { continue; }
            let node = Node { name, path: p.clone(), size, is_dir: false };
            if tx.send(Msg::Entry { parent: path.to_path_buf(), node }).is_err() { return; }
            repaint();
        }
    }
}
