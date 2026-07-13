## Problem

When a thread with a custom `thread_path` is closed (e.g. GitHub issue closed), the thread directory is deleted from disk. However, the `thread_paths` in-memory mapping is never cleaned up. As a result, `list_threads()` keeps returning the deleted thread — it shows as an orphan in the dashboard thread list until the process restarts.

### Root Cause

In `list_threads()`, the custom `thread_paths` entries were included **without checking if the directory still exists on disk**:

```rust
let paths = self.thread_paths.lock().await;
for name in paths.keys() {
    if !thread_names.contains(name) {
        thread_names.push(name.clone());  // no filesystem check!
    }
}
```

For workspace threads, the directory scan naturally skips deleted directories. But custom `thread_path` threads live outside the workspace, so the scan never finds them — they only come from the `thread_paths` map, which is never pruned.

### Fix

Use `retain()` to prune `thread_paths` entries whose `.jyc/` directory no longer exists:

```rust
paths.retain(|_name, path| path.join(".jyc").is_dir());
```

This runs every time `list_threads()` is called, so orphan entries are cleaned up immediately when the dashboard next refreshes.

### Test

Added `test_list_threads_cleans_stale_custom_path` — inserts a custom path into `thread_paths`, deletes the directory, calls `list_threads()`, asserts the thread is no longer listed.

### Checklist

- [x] cargo fmt --check — pass
- [x] cargo clippy --workspace -- -D warnings — pass
- [x] Unit test added and passing
- [x] CHANGELOG.md updated
