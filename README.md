# SpaceTreeMap

Windows NTFS disk-space visualizer. On Windows it reads the volume's `$MFT` sequentially and builds a path index without recursively opening every directory. The result is a snapshot; scan again to refresh after files change.

Linux and macOS CI builds are preview shells that explain the Windows-only indexing limitation; raw NTFS MFT access and native Shell operations are not implemented on those platforms.

## Run

Requires Windows 10 or later, Rust stable x64 MSVC, and the Windows SDK. The application requests administrator elevation because Windows requires it for raw volume access.

```powershell
.uild.ps1
```

The script isolates Cargo configuration and target files in `.cargo-home` and `target`. Run `cargo test --lib` with the same `CARGO_HOME` setting to execute unit tests. `--demo` opens a UI demonstration with synthetic data and no disk operations:

```powershell
& .\target\release\space_tree_map.exe --demo
```

## Use

Select an NTFS drive and choose **扫描 / 刷新**. Scanning runs in a worker thread and can be cancelled. Double-click a directory to zoom in, use the breadcrumb or back/up controls to navigate, and double-click a file to open it. Right-click a file or directory for the native Windows Shell context menu, including installed Shell extensions. Renaming is handled by the application after selecting the Shell rename command. Shell changes do not alter the snapshot; rescan to refresh it.

The treemap uses logical file sizes. Hard links count once for every displayed path, which matches path-based disk usage views but can make the total exceed allocated volume usage. Zero-byte items and empty directories remain available in the item list. MFT records whose parent links cannot be validated appear under **[无法还原路径]** and cannot invoke Shell operations.

## Scope and limitations

- One fixed or removable NTFS volume per scan; no USN Journal or incremental updates.
- Raw volume access and Shell COM are Windows-specific.
- `$MFT` fragmented extents are resolved by the `ntfs` crate. Attribute-list extension records are merged into their base records.
- Directory sizes are the sum of descendant unnamed `$DATA` logical sizes; alternate data streams are not included.
- The application skips deleted MFT entries and never reads file payloads while building the index.

## Validation

Unit tests cover the treemap layout, hard links, Unicode paths, stale/cyclic parents, cancellation, truncated records, and aligned raw-reader caching. Actual scanning and Shell extension compatibility should be validated on an elevated Windows session with representative NTFS volumes.
