use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::BTreeSet;

use super::context_packets::{
    get_packet_summary, get_retrieval_event, list_visible_packet_items, ContextPacketItemSummary,
    ContextPacketSummary, RetrievalEventSummary,
};

const MAX_HANDOFF_PACKET_IDS: usize = 8;
const MAX_HANDOFF_RETRIEVAL_IDS: usize = 8;
const MAX_HANDOFF_ITEMS_PER_PACKET: usize = 8;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ContextHandoffRequest {
    #[serde(default)]
    pub context_packet_ids: Vec<String>,
    #[serde(default)]
    pub retrieval_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResolvedContextHandoff {
    pub packets: Vec<ResolvedContextPacket>,
    pub retrievals: Vec<RetrievalEventSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResolvedContextPacket {
    #[serde(flatten)]
    pub summary: ContextPacketSummary,
    pub items: Vec<ContextPacketItemSummary>,
}

impl ContextHandoffRequest {
    pub(crate) fn has_handles(&self) -> bool {
        self.context_packet_ids
            .iter()
            .any(|id| !id.trim().is_empty())
            || self.retrieval_ids.iter().any(|id| !id.trim().is_empty())
    }
}

impl ResolvedContextHandoff {
    pub(crate) fn packet_ids(&self) -> Vec<String> {
        self.packets
            .iter()
            .map(|packet| packet.summary.packet_id.clone())
            .collect()
    }

    pub(crate) fn retrieval_ids(&self) -> Vec<String> {
        self.retrievals
            .iter()
            .map(|retrieval| retrieval.retrieval_id.clone())
            .collect()
    }

    pub(crate) fn prompt_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize artifact-memory handoff")
    }

    pub(crate) fn metadata_json(&self) -> Value {
        json!({
            "context_packet_ids": self.packet_ids(),
            "retrieval_ids": self.retrieval_ids(),
            "packet_count": self.packets.len(),
            "retrieval_count": self.retrievals.len(),
        })
    }
}

pub(crate) async fn resolve_context_handoff(
    pool: &SqlitePool,
    organization: &str,
    request: &ContextHandoffRequest,
) -> Result<Option<ResolvedContextHandoff>> {
    let packet_ids = normalize_handles(&request.context_packet_ids, MAX_HANDOFF_PACKET_IDS)
        .context("normalize context_packet_ids")?;
    let requested_retrieval_ids =
        normalize_handles(&request.retrieval_ids, MAX_HANDOFF_RETRIEVAL_IDS)
            .context("normalize retrieval_ids")?;

    if packet_ids.is_empty() && requested_retrieval_ids.is_empty() {
        return Ok(None);
    }

    let mut packets = Vec::with_capacity(packet_ids.len());
    let mut retrieval_ids = requested_retrieval_ids;
    for packet_id in packet_ids {
        let summary = get_packet_summary(pool, organization, &packet_id)
            .await
            .with_context(|| format!("load context packet {packet_id}"))?
            .with_context(|| format!("context packet not found or not visible: {packet_id}"))?;
        if let Some(retrieval_id) = summary.retrieval_id.as_deref() {
            push_unique(&mut retrieval_ids, retrieval_id.to_string());
        }

        let mut items = list_visible_packet_items(pool, organization, &packet_id)
            .await
            .with_context(|| format!("load context packet items {packet_id}"))?;
        if items.len() > MAX_HANDOFF_ITEMS_PER_PACKET {
            items.truncate(MAX_HANDOFF_ITEMS_PER_PACKET);
        }
        packets.push(ResolvedContextPacket { summary, items });
    }

    if retrieval_ids.len() > MAX_HANDOFF_RETRIEVAL_IDS {
        bail!(
            "too many retrieval_ids after packet expansion: {} > {}",
            retrieval_ids.len(),
            MAX_HANDOFF_RETRIEVAL_IDS
        );
    }

    let mut retrievals = Vec::with_capacity(retrieval_ids.len());
    for retrieval_id in retrieval_ids {
        let retrieval = get_retrieval_event(pool, organization, &retrieval_id)
            .await
            .with_context(|| format!("load retrieval trace {retrieval_id}"))?
            .with_context(|| format!("retrieval trace not found or not visible: {retrieval_id}"))?;
        retrievals.push(retrieval);
    }

    Ok(Some(ResolvedContextHandoff {
        packets,
        retrievals,
    }))
}

fn normalize_handles(raw: &[String], max: usize) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut values = Vec::new();
    for value in raw {
        let trimmed = value.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        values.push(trimmed.to_string());
    }
    if values.len() > max {
        bail!("too many handles: {} > {}", values.len(), max);
    }
    Ok(values)
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use sqlx::sqlite::SqlitePoolOptions;

    const ORG: &str = "agentic-flowstate";
    const PACKET_ID: &str = "CP-HANDOFF1234";
    const RETRIEVAL_ID: &str = "R-HANDOFF1234";
    const ARTIFACT_ID: &str = "A-HANDOFF1234";
    const CHUNK_ID: &str = "C-HANDOFF1234-1";
    const RAW_PARENT_TRANSCRIPT_SENTINEL: &str =
        "RAW_PARENT_TRANSCRIPT_SENTINEL full parent transcript text";
    const FULL_OUTPUT_SENTINEL: &str = "FULL_OUTPUT_SENTINEL broad artifact output body";
    const PRIVATE_PACKET_SENTINEL: &str = "PRIVATE_PACKET_SENTINEL should not flow";
    const BOUNDED_SNIPPET: &str =
        "Bounded packet snippet with the handoff citation and next action.";

    async fn handoff_test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");

        for statement in [
            r#"CREATE TABLE artifacts (
                artifact_id TEXT PRIMARY KEY,
                title TEXT,
                content TEXT,
                organization TEXT NOT NULL,
                lifecycle_status TEXT NOT NULL DEFAULT 'active',
                visibility TEXT NOT NULL DEFAULT 'organization'
            )"#,
            r#"CREATE TABLE artifact_chunks (
                chunk_id TEXT PRIMARY KEY,
                artifact_id TEXT NOT NULL,
                lifecycle_status TEXT NOT NULL DEFAULT 'active'
            )"#,
            r#"CREATE TABLE documents (
                document_id TEXT PRIMARY KEY,
                organization TEXT NOT NULL
            )"#,
            r#"CREATE TABLE tickets (
                ticket_id TEXT PRIMARY KEY,
                organization TEXT NOT NULL
            )"#,
            r#"CREATE TABLE knowledge_items (
                knowledge_id TEXT PRIMARY KEY,
                organization TEXT NOT NULL,
                lifecycle_status TEXT NOT NULL DEFAULT 'active',
                visibility TEXT NOT NULL DEFAULT 'organization'
            )"#,
            r#"CREATE TABLE memory_entities (
                entity_id TEXT PRIMARY KEY,
                organization TEXT NOT NULL
            )"#,
            r#"CREATE TABLE retrieval_events (
                retrieval_id TEXT PRIMARY KEY,
                organization TEXT NOT NULL,
                actor_type TEXT NOT NULL,
                actor_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                work_summary TEXT,
                query_text TEXT NOT NULL,
                normalized_query TEXT,
                filters_json TEXT NOT NULL DEFAULT '{}',
                authorization_filter_json TEXT NOT NULL DEFAULT '{}',
                strategy TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                elapsed_ms INTEGER NOT NULL,
                result_count INTEGER NOT NULL,
                selected_count INTEGER NOT NULL,
                empty_result INTEGER NOT NULL DEFAULT 0,
                context_token_count INTEGER,
                context_truncated INTEGER NOT NULL DEFAULT 0,
                warnings_json TEXT NOT NULL DEFAULT '[]',
                metadata_json TEXT NOT NULL DEFAULT '{}'
            )"#,
            r#"CREATE TABLE context_packets (
                packet_id TEXT PRIMARY KEY,
                organization TEXT NOT NULL,
                ticket_id TEXT,
                repository TEXT,
                namespace_id TEXT,
                work_summary TEXT NOT NULL,
                created_by TEXT NOT NULL,
                created_by_agent TEXT,
                retrieval_id TEXT,
                query_plan_json TEXT NOT NULL DEFAULT '{}',
                summary TEXT NOT NULL,
                warnings_json TEXT NOT NULL DEFAULT '[]',
                token_budget INTEGER,
                token_count INTEGER,
                created_at INTEGER NOT NULL,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            )"#,
            r#"CREATE TABLE context_packet_items (
                packet_id TEXT NOT NULL,
                rank INTEGER NOT NULL,
                item_type TEXT NOT NULL,
                artifact_id TEXT,
                chunk_id TEXT,
                knowledge_id TEXT,
                ticket_id TEXT,
                document_id TEXT,
                entity_id TEXT,
                citation_label TEXT,
                relevance_reason TEXT NOT NULL,
                included_text TEXT,
                token_count INTEGER,
                source_retrieval_rank INTEGER,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                PRIMARY KEY (packet_id, rank)
            )"#,
        ] {
            sqlx::query(statement)
                .execute(&pool)
                .await
                .expect("create handoff test table");
        }

        sqlx::query(
            r#"
            INSERT INTO artifacts (
                artifact_id, title, content, organization, lifecycle_status, visibility
            )
            VALUES (?, 'Handoff Source', ?, ?, 'active', 'organization')
            "#,
        )
        .bind(ARTIFACT_ID)
        .bind(format!(
            "{RAW_PARENT_TRANSCRIPT_SENTINEL}\n{FULL_OUTPUT_SENTINEL}"
        ))
        .bind(ORG)
        .execute(&pool)
        .await
        .expect("insert visible artifact");

        sqlx::query(
            r#"
            INSERT INTO artifacts (
                artifact_id, title, content, organization, lifecycle_status, visibility
            )
            VALUES ('A-PRIVATE1234', 'Private Source', ?, ?, 'active', 'private')
            "#,
        )
        .bind(PRIVATE_PACKET_SENTINEL)
        .bind(ORG)
        .execute(&pool)
        .await
        .expect("insert private artifact");

        sqlx::query(
            "INSERT INTO artifact_chunks (chunk_id, artifact_id, lifecycle_status) VALUES (?, ?, 'active')",
        )
        .bind(CHUNK_ID)
        .bind(ARTIFACT_ID)
        .execute(&pool)
        .await
        .expect("insert chunk");

        sqlx::query(
            r#"
            INSERT INTO retrieval_events (
                retrieval_id, organization, actor_type, actor_id, tool_name,
                work_summary, query_text, normalized_query, filters_json,
                authorization_filter_json, strategy, started_at, elapsed_ms,
                result_count, selected_count, empty_result, context_token_count,
                context_truncated, warnings_json, metadata_json
            )
            VALUES (?, ?, 'agent', 'handoff-test', 'gather_context',
                'runner handoff smoke', 'packet handoff query',
                'packet handoff query', ?, ?, 'fts_facets_links_v1',
                1781662200, 17, 2, 1, 0, 64, 1, ?, ?)
            "#,
        )
        .bind(RETRIEVAL_ID)
        .bind(ORG)
        .bind(json!({"ticket_id": "T-HANDOFF12"}).to_string())
        .bind(json!({"organization": ORG, "visibility": ["organization", "system"]}).to_string())
        .bind(json!(["packet_truncated"]).to_string())
        .bind(json!({"query_terms": ["packet", "handoff"]}).to_string())
        .execute(&pool)
        .await
        .expect("insert retrieval event");

        sqlx::query(
            r#"
            INSERT INTO context_packets (
                packet_id, organization, ticket_id, repository, work_summary,
                created_by, created_by_agent, retrieval_id, summary,
                warnings_json, token_budget, token_count, created_at, metadata_json
            )
            VALUES (?, ?, 'T-HANDOFF12', 'agentic-flowstate-api',
                'handoff work summary', 'handoff-test',
                'conversation-child-agent', ?, 'stored packet summary',
                ?, 2000, 64, 1781662201, ?)
            "#,
        )
        .bind(PACKET_ID)
        .bind(ORG)
        .bind(RETRIEVAL_ID)
        .bind(json!(["packet_truncated"]).to_string())
        .bind(json!({"source": "api-context-gather"}).to_string())
        .execute(&pool)
        .await
        .expect("insert context packet");

        sqlx::query(
            r#"
            INSERT INTO context_packet_items (
                packet_id, rank, item_type, artifact_id, chunk_id,
                citation_label, relevance_reason, included_text,
                token_count, source_retrieval_rank, metadata_json
            )
            VALUES (?, 1, 'chunk', ?, ?, 'A-HANDOFF1234#C-HANDOFF1234-1',
                'selected retrieval chunk', ?, 12, 1, ?)
            "#,
        )
        .bind(PACKET_ID)
        .bind(ARTIFACT_ID)
        .bind(CHUNK_ID)
        .bind(BOUNDED_SNIPPET)
        .bind(json!({"matched_fields": ["content"]}).to_string())
        .execute(&pool)
        .await
        .expect("insert visible packet item");

        sqlx::query(
            r#"
            INSERT INTO context_packet_items (
                packet_id, rank, item_type, artifact_id, citation_label,
                relevance_reason, included_text, token_count, metadata_json
            )
            VALUES (?, 2, 'artifact', 'A-PRIVATE1234', 'A-PRIVATE1234',
                'private source should be filtered', ?, 8, '{}')
            "#,
        )
        .bind(PACKET_ID)
        .bind(PRIVATE_PACKET_SENTINEL)
        .execute(&pool)
        .await
        .expect("insert private packet item");

        sqlx::query(
            r#"
            INSERT INTO context_packet_items (
                packet_id, rank, item_type, relevance_reason,
                included_text, token_count, metadata_json
            )
            VALUES (?, 3, 'warning', 'packet warning',
                'Packet was truncated before handoff.', 6, '{}')
            "#,
        )
        .bind(PACKET_ID)
        .execute(&pool)
        .await
        .expect("insert warning packet item");

        pool
    }

    #[tokio::test]
    async fn resolve_context_handoff_preserves_packet_snippets_and_trace_metadata_without_raw_dumps(
    ) {
        let pool = handoff_test_pool().await;
        let handoff = resolve_context_handoff(
            &pool,
            ORG,
            &ContextHandoffRequest {
                context_packet_ids: vec![format!("  {PACKET_ID}  ")],
                retrieval_ids: vec![RETRIEVAL_ID.to_string(), RETRIEVAL_ID.to_string()],
            },
        )
        .await
        .expect("resolve handoff")
        .expect("handoff exists");

        assert_eq!(handoff.packet_ids(), vec![PACKET_ID.to_string()]);
        assert_eq!(handoff.retrieval_ids(), vec![RETRIEVAL_ID.to_string()]);
        assert_eq!(handoff.packets[0].items.len(), 2);
        assert_eq!(
            handoff.packets[0].items[0].included_text.as_deref(),
            Some(BOUNDED_SNIPPET)
        );
        assert_eq!(
            handoff.packets[0].items[0].citation_label.as_deref(),
            Some("A-HANDOFF1234#C-HANDOFF1234-1")
        );
        assert_eq!(handoff.retrievals[0].selected_count, 1);
        assert!(handoff.retrievals[0].context_truncated);
        assert_eq!(
            handoff.retrievals[0].authorization_filter["organization"],
            ORG
        );

        let prompt_json = handoff.prompt_json().expect("serialize prompt handoff");
        assert!(prompt_json.contains(PACKET_ID));
        assert!(prompt_json.contains(RETRIEVAL_ID));
        assert!(prompt_json.contains(BOUNDED_SNIPPET));
        assert!(prompt_json.contains("packet_truncated"));
        assert!(!prompt_json.contains(RAW_PARENT_TRANSCRIPT_SENTINEL));
        assert!(!prompt_json.contains(FULL_OUTPUT_SENTINEL));
        assert!(!prompt_json.contains(PRIVATE_PACKET_SENTINEL));

        let prompt_value: Value = serde_json::from_str(&prompt_json).expect("handoff json");
        assert_eq!(prompt_value["packets"][0]["packet_id"], PACKET_ID);
        assert_eq!(prompt_value["packets"][0]["retrieval_id"], RETRIEVAL_ID);
        assert_eq!(
            prompt_value["packets"][0]["items"][0]["included_text"],
            BOUNDED_SNIPPET
        );
        assert_eq!(
            prompt_value["retrievals"][0]["filters"]["ticket_id"],
            "T-HANDOFF12"
        );
        assert_eq!(
            prompt_value["retrievals"][0]["metadata"]["query_terms"][0],
            "packet"
        );

        let metadata = handoff.metadata_json();
        assert_eq!(metadata["context_packet_ids"], json!([PACKET_ID]));
        assert_eq!(metadata["retrieval_ids"], json!([RETRIEVAL_ID]));
        assert_eq!(metadata["packet_count"], 1);
        assert_eq!(metadata["retrieval_count"], 1);
        assert!(metadata.get("packets").is_none());
        assert!(metadata.get("items").is_none());
        assert!(metadata.get("included_text").is_none());
        assert!(!metadata.to_string().contains(BOUNDED_SNIPPET));
        assert!(!metadata.to_string().contains("packet handoff query"));
    }
}
