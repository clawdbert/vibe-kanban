use std::sync::Arc;

use futures::StreamExt;
use workspace_utils::msg_store::MsgStore;

use crate::logs::{
    NormalizedEntry, NormalizedEntryType,
    plain_text_processor::PlainTextLogProcessor,
    utils::EntryIndexProvider,
};

/// ClawdBert log normalizer.
///
/// ClawdBert dispatches to an OpenClaw gateway session, which in turn may
/// launch Claude Code or other coding agents. The dispatch script is expected
/// to forward the underlying agent's stream-json output on stdout.
///
/// For now we treat all stdout as plain-text system messages. A future
/// improvement can parse Claude Code stream-json events if the dispatch
/// script passes them through.
pub fn normalize_logs(
    msg_store: Arc<MsgStore>,
    _worktree_path: &std::path::Path,
    entry_index_provider: EntryIndexProvider,
) {
    // Normalize stderr
    normalize_stderr(msg_store.clone(), entry_index_provider.clone());

    // Normalize stdout — treat as system messages for now
    let store = msg_store.clone();
    let idx = entry_index_provider.clone();
    tokio::spawn(async move {
        let mut lines = store
            .stdout_lines_stream()
            .filter_map(|r| futures::future::ready(r.ok()));

        while let Some(line) = lines.next().await {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let entry = NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::SystemMessage,
                content: strip_ansi_escapes::strip_str(trimmed).to_string(),
                metadata: None,
            };

            crate::logs::utils::patch::add_normalized_entry(&store, &idx, entry);
        }
    });
}

fn normalize_stderr(msg_store: Arc<MsgStore>, entry_index_provider: EntryIndexProvider) {
    tokio::spawn(async move {
        let mut stderr = msg_store.stderr_chunked_stream();

        let mut processor = PlainTextLogProcessor::builder()
            .normalized_entry_producer(Box::new(|content: String| NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ErrorMessage {
                    error_type: crate::logs::NormalizedEntryError::Other,
                },
                content,
                metadata: None,
            }))
            .transform_lines(Box::new(|lines| {
                lines.iter_mut().for_each(|line| {
                    *line = strip_ansi_escapes::strip_str(&line);
                });
            }))
            .time_gap(std::time::Duration::from_secs(2))
            .index_provider(entry_index_provider)
            .build();

        while let Some(Ok(chunk)) = stderr.next().await {
            for patch in processor.process(chunk) {
                msg_store.push_patch(patch);
            }
        }
    });
}
