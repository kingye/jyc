use anyhow::Result;
use std::path::Path;

/// Show current monitoring state for all channels.
pub async fn run(workdir: &Path) -> Result<()> {
    // Read per-channel state files from <channel>/.imap/.state.json
    let mut found = false;

    let mut entries = tokio::fs::read_dir(workdir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let state_file = path.join(".imap").join(".state.json");
        if state_file.exists() {
            found = true;
            let channel_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();

            let content = tokio::fs::read_to_string(&state_file).await?;
            let state: serde_json::Value = serde_json::from_str(&content)?;

            println!("Channel: {channel_name}");
            if let Some(seq) = state.get("last_sequence_number") {
                println!("  Last sequence number: {seq}");
            }
            if let Some(uid) = state.get("last_processed_uid") {
                println!("  Last processed UID: {uid}");
            }
            if let Some(ts) = state.get("last_processed_timestamp") {
                println!("  Last processed: {ts}");
            }
            if let Some(validity) = state.get("uid_validity") {
                println!("  UID validity: {validity}");
            }
            println!();
        }
    }

    if !found {
        println!("No monitoring state found. Run 'jyc monitor' first.");
    }

    Ok(())
}
