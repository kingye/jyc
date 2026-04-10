use std::path::Path;
use walkdir::WalkDir;

/// Copy template files from source to target directory.
/// Skips files that already exist in the target.
/// Returns the number of files copied.
pub async fn copy_template_files(
    template_src: &Path,
    target_dir: &Path,
) -> anyhow::Result<usize> {
    tokio::fs::create_dir_all(target_dir).await?;
    
    let mut copied = 0;
    for entry in WalkDir::new(template_src).into_iter().filter_map(|e| e.ok()) {
        let relative = entry.path().strip_prefix(template_src)?;
        let target = target_dir.join(relative);
        
        if target.exists() {
            continue;
        }
        
        if entry.file_type().is_dir() {
            tokio::fs::create_dir_all(&target).await?;
        } else {
            tokio::fs::copy(entry.path(), &target).await?;
            copied += 1;
        }
    }
    
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_copy_template_files() {
        let tmp = tempfile::tempdir().unwrap();
        
        // Create source template directory
        let src = tmp.path().join("template");
        tokio::fs::create_dir_all(&src).await.unwrap();
        tokio::fs::write(src.join("file1.txt"), "content1").await.unwrap();
        tokio::fs::write(src.join("file2.txt"), "content2").await.unwrap();
        
        // Create target directory
        let target = tmp.path().join("target");
        
        // Copy files
        let copied = copy_template_files(&src, &target).await.unwrap();
        
        assert_eq!(copied, 2);
        assert!(target.join("file1.txt").exists());
        assert!(target.join("file2.txt").exists());
        assert_eq!(tokio::fs::read_to_string(target.join("file1.txt")).await.unwrap(), "content1");
    }
    
    #[tokio::test]
    async fn test_copy_skips_existing_files() {
        let tmp = tempfile::tempdir().unwrap();
        
        // Create source
        let src = tmp.path().join("template");
        tokio::fs::create_dir_all(&src).await.unwrap();
        tokio::fs::write(src.join("file.txt"), "new content").await.unwrap();
        
        // Create target with existing file
        let target = tmp.path().join("target");
        tokio::fs::create_dir_all(&target).await.unwrap();
        tokio::fs::write(target.join("file.txt"), "existing content").await.unwrap();
        
        let copied = copy_template_files(&src, &target).await.unwrap();
        
        assert_eq!(copied, 0);
        // Original file should be preserved
        assert_eq!(tokio::fs::read_to_string(target.join("file.txt")).await.unwrap(), "existing content");
    }
}