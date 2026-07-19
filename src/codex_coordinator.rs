use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::Serialize;
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use ticketing_system::{conversations, Conversation};

use crate::agents::AgentType;

pub const ALEX_USER_ID: &str = "alex";
pub const CODEX_COORDINATOR_CONVERSATION_TYPE: &str = "codex_coordinator";
pub const CODEX_COORDINATOR_AGENT: &str = "codex-coordinator";
pub const CODEX_COORDINATOR_PROMPT_VERSION: &str = "codex-coordinator/v1";
pub const CODEX_COORDINATOR_ORGANIZATION: &str = "agentic-flowstate";
pub const CODEX_COORDINATOR_TITLE: &str = "Alex";

const CODEX_AUTH_METHOD: &str = "chatgpt";
const LEGACY_FABLE_CONVERSATION_TYPE: &str = "fable_coordinator";
const LEGACY_FABLE_AGENT: &str = "fable-coordinator";

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct CodexCoordinatorRuntimeState {
    pub conversation_id: String,
    pub codex_thread_id: Option<String>,
    pub thread_state: String,
    pub prompt_version: String,
    pub model: String,
    pub effort: String,
    pub auth_method: String,
    pub last_started_at: Option<i64>,
    pub last_completed_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub last_turn_duration_ms: Option<i64>,
    pub last_tool_call_count: i32,
    pub last_terminal_status: Option<String>,
    pub last_error_class: Option<String>,
    pub last_wake_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct CodexCoordinatorThreadPlan {
    pub resume_thread_id: Option<String>,
    pub rehydrate_required: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexCoordinatorThreadRepair {
    pub retired_thread_id: Option<String>,
    pub rehydrate_required: bool,
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool
        .begin()
        .await
        .context("begin Codex coordinator schema migration")?;
    let schema_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'codex_coordinator_runtime'",
    )
    .fetch_one(&mut *tx)
    .await
    .context("inspect Codex coordinator runtime schema")?;

    if schema_exists == 0 {
        let migrated = sqlx::query(
            r#"
            UPDATE conversations
            SET agent = ?, conversation_type = ?, session_id = NULL, updated_at = ?
            WHERE user_id = ?
              AND (
                agent IN (?, ?)
                OR conversation_type IN (?, ?)
              )
            "#,
        )
        .bind(CODEX_COORDINATOR_AGENT)
        .bind(CODEX_COORDINATOR_CONVERSATION_TYPE)
        .bind(Utc::now().to_rfc3339())
        .bind(ALEX_USER_ID)
        .bind(LEGACY_FABLE_AGENT)
        .bind(CODEX_COORDINATOR_AGENT)
        .bind(LEGACY_FABLE_CONVERSATION_TYPE)
        .bind(CODEX_COORDINATOR_CONVERSATION_TYPE)
        .execute(&mut *tx)
        .await
        .context("migrate Alex to the Codex coordinator designation")?
        .rows_affected();

        sqlx::query("DROP TABLE IF EXISTS fable_codex_events")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS fable_codex_runtime")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS fable_coordinator_events")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS fable_coordinator_runtime")
            .execute(&mut *tx)
            .await?;
        sqlx::query("DROP INDEX IF EXISTS idx_fable_coordinator_singleton")
            .execute(&mut *tx)
            .await?;

        tracing::info!(
            event = "codex_coordinator.runtime_migrated",
            migrated_conversation_count = migrated,
            "migrated Alex to the Codex-only coordinator runtime"
        );
    }

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS codex_coordinator_runtime (
            conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE RESTRICT,
            codex_thread_id TEXT UNIQUE,
            thread_state TEXT NOT NULL CHECK (
                thread_state IN ('starting', 'ready', 'repair_required')
            ),
            prompt_version TEXT NOT NULL,
            model TEXT NOT NULL,
            effort TEXT NOT NULL,
            auth_method TEXT NOT NULL CHECK (auth_method = 'chatgpt'),
            last_started_at INTEGER,
            last_completed_at INTEGER,
            last_success_at INTEGER,
            last_turn_duration_ms INTEGER,
            last_tool_call_count INTEGER NOT NULL DEFAULT 0,
            last_terminal_status TEXT,
            last_error_class TEXT,
            last_wake_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create Codex coordinator runtime table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS codex_coordinator_events (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE RESTRICT,
            event_type TEXT NOT NULL,
            thread_id TEXT,
            status TEXT,
            detail TEXT,
            created_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("create Codex coordinator event table")?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_codex_coordinator_events_conversation_created ON codex_coordinator_events(conversation_id, created_at DESC)",
    )
    .execute(&mut *tx)
    .await
    .context("index Codex coordinator events")?;
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_codex_coordinator_singleton
        ON conversations(user_id)
        WHERE conversation_type = 'codex_coordinator'
        "#,
    )
    .execute(&mut *tx)
    .await
    .context("enforce one coordinator conversation per user")?;

    tx.commit()
        .await
        .context("commit Codex coordinator schema migration")?;
    Ok(())
}

pub fn require_alex(user_id: &str) -> Result<()> {
    if user_id == ALEX_USER_ID {
        Ok(())
    } else {
        Err(anyhow!("The permanent Codex coordinator is owned by Alex"))
    }
}

pub fn is_codex_coordinator_conversation(conversation: &Conversation) -> bool {
    conversation.conversation_type.as_deref() == Some(CODEX_COORDINATOR_CONVERSATION_TYPE)
}

pub fn validate_runtime_assignment(
    conversation: &Conversation,
    codex_coordinator_agent: bool,
) -> Result<()> {
    let has_codex_coordinator_type =
        conversation.conversation_type.as_deref() == Some(CODEX_COORDINATOR_CONVERSATION_TYPE);
    let has_codex_coordinator_agent =
        conversation.agent.as_deref() == Some(CODEX_COORDINATOR_AGENT);
    let designated = has_codex_coordinator_type && has_codex_coordinator_agent;

    if codex_coordinator_agent {
        if !designated {
            return Err(anyhow!(
                "The Codex coordinator agent is reserved for Alex's permanent coordinator"
            ));
        }
        return validate_singleton(conversation);
    }

    if has_codex_coordinator_type || has_codex_coordinator_agent {
        return Err(anyhow!(
            "Alex's permanent coordinator must run through the Codex coordinator agent"
        ));
    }
    Ok(())
}

pub async fn ensure_singleton(pool: &SqlitePool, user_id: &str) -> Result<Conversation> {
    require_alex(user_id)?;
    ensure_schema(pool).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let now_iso = Utc::now().to_rfc3339();
    sqlx::query(
        r#"
        INSERT OR IGNORE INTO conversations (
            id, user_id, session_id, organization, agent, conversation_type,
            parent_conversation_id, conversation_role, child_sort_order,
            title, started_at, updated_at, status
        )
        VALUES (?, ?, NULL, ?, ?, ?, NULL, 'multi_agent_parent', NULL, ?, ?, ?, 'open')
        "#,
    )
    .bind(&id)
    .bind(user_id)
    .bind(CODEX_COORDINATOR_ORGANIZATION)
    .bind(CODEX_COORDINATOR_AGENT)
    .bind(CODEX_COORDINATOR_CONVERSATION_TYPE)
    .bind(CODEX_COORDINATOR_TITLE)
    .bind(&now_iso)
    .bind(&now_iso)
    .execute(pool)
    .await
    .context("create permanent Codex coordinator conversation")?;

    let conversation_id: String = sqlx::query_scalar(
        "SELECT id FROM conversations WHERE user_id = ? AND conversation_type = ?",
    )
    .bind(user_id)
    .bind(CODEX_COORDINATOR_CONVERSATION_TYPE)
    .fetch_one(pool)
    .await
    .context("load permanent Codex coordinator conversation id")?;
    let conversation = conversations::get_conversation(pool, &conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("Permanent Codex coordinator disappeared after creation"))?;
    validate_singleton(&conversation)?;
    Ok(conversation)
}

fn validate_singleton(conversation: &Conversation) -> Result<()> {
    if conversation.user_id != ALEX_USER_ID
        || conversation.agent.as_deref() != Some(CODEX_COORDINATOR_AGENT)
        || conversation.conversation_type.as_deref() != Some(CODEX_COORDINATOR_CONVERSATION_TYPE)
        || conversation.parent_conversation_id.is_some()
        || conversation.conversation_role != "multi_agent_parent"
        || conversation.status != "open"
        || conversation.archived_at.is_some()
    {
        return Err(anyhow!(
            "Permanent Codex coordinator {} has invalid durable designation; explicit repair is required",
            conversation.id
        ));
    }
    Ok(())
}

pub async fn runtime_state(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<CodexCoordinatorRuntimeState>> {
    sqlx::query_as::<_, CodexCoordinatorRuntimeState>(
        r#"
        SELECT conversation_id, codex_thread_id, thread_state, prompt_version,
               model, effort, auth_method, last_started_at, last_completed_at,
               last_success_at, last_turn_duration_ms, last_tool_call_count,
               last_terminal_status, last_error_class, last_wake_at,
               created_at, updated_at
        FROM codex_coordinator_runtime
        WHERE conversation_id = ?
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
    .context("load Codex coordinator runtime state")
}

fn runtime_policy_matches(state: &CodexCoordinatorRuntimeState) -> bool {
    state.prompt_version == CODEX_COORDINATOR_PROMPT_VERSION
        && state.model == AgentType::CodexCoordinator.model()
        && state.effort == AgentType::CodexCoordinator.effort()
        && state.auth_method == CODEX_AUTH_METHOD
}

pub async fn prepare_thread(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<CodexCoordinatorThreadPlan> {
    ensure_schema(pool).await?;
    let conversation = conversations::get_conversation(pool, conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("Codex coordinator conversation not found"))?;
    validate_singleton(&conversation)?;

    let Some(state) = runtime_state(pool, conversation_id).await? else {
        if conversation.session_id.is_some() {
            return Err(anyhow!(
                "The Codex coordinator has an untracked thread; explicit repair is required"
            ));
        }
        return Ok(CodexCoordinatorThreadPlan {
            resume_thread_id: None,
            rehydrate_required: true,
        });
    };

    if conversation.session_id != state.codex_thread_id {
        return Err(anyhow!(
            "Codex coordinator thread metadata diverged from the canonical conversation; explicit repair is required"
        ));
    }
    if !runtime_policy_matches(&state) {
        require_thread_repair(pool, conversation_id, "runtime_policy_changed").await?;
        return Err(anyhow!(
            "Codex coordinator runtime policy changed; explicit repair is required"
        ));
    }

    match state.thread_state.as_str() {
        "ready" => Ok(CodexCoordinatorThreadPlan {
            resume_thread_id: state.codex_thread_id,
            rehydrate_required: false,
        }),
        "starting" => Err(anyhow!(
            "Codex coordinator thread startup was interrupted; explicit repair is required"
        )),
        "repair_required" => Err(anyhow!(
            "Codex coordinator thread repair is required before another coordinator turn"
        )),
        other => Err(anyhow!(
            "Unsupported Codex coordinator thread state: {other}"
        )),
    }
}

pub async fn record_turn_started(
    pool: &SqlitePool,
    conversation_id: &str,
    plan: &CodexCoordinatorThreadPlan,
    wake: bool,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let model = AgentType::CodexCoordinator.model();
    let effort = AgentType::CodexCoordinator.effort();
    let mut tx = pool.begin().await?;

    let existing_state: Option<String> = sqlx::query_scalar(
        "SELECT thread_state FROM codex_coordinator_runtime WHERE conversation_id = ?",
    )
    .bind(conversation_id)
    .fetch_optional(&mut *tx)
    .await?;
    if existing_state
        .as_deref()
        .is_some_and(|state| state != "ready")
    {
        return Err(anyhow!(
            "Codex coordinator thread is not ready to start another turn"
        ));
    }

    sqlx::query(
        r#"
        INSERT INTO codex_coordinator_runtime (
            conversation_id, codex_thread_id, thread_state, prompt_version,
            model, effort, auth_method, last_started_at, last_wake_at,
            created_at, updated_at
        ) VALUES (?, ?, 'starting', ?, ?, ?, 'chatgpt', ?, ?, ?, ?)
        ON CONFLICT(conversation_id) DO UPDATE SET
            thread_state = 'starting', prompt_version = excluded.prompt_version,
            model = excluded.model, effort = excluded.effort,
            auth_method = excluded.auth_method, last_started_at = excluded.last_started_at,
            last_wake_at = COALESCE(excluded.last_wake_at, codex_coordinator_runtime.last_wake_at),
            last_error_class = NULL, updated_at = excluded.updated_at
        "#,
    )
    .bind(conversation_id)
    .bind(plan.resume_thread_id.as_deref())
    .bind(CODEX_COORDINATOR_PROMPT_VERSION)
    .bind(model)
    .bind(effort)
    .bind(now)
    .bind(wake.then_some(now))
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    insert_event_tx(
        &mut tx,
        conversation_id,
        "turn_started",
        plan.resume_thread_id.as_deref(),
        Some("starting"),
        wake.then_some("coordinator_wake"),
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn mark_thread_ready(
    pool: &SqlitePool,
    conversation_id: &str,
    thread_id: &str,
) -> Result<()> {
    if thread_id.trim().is_empty() {
        return Err(anyhow!("Codex returned an empty coordinator thread id"));
    }
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    let existing_session: Option<String> =
        sqlx::query_scalar("SELECT session_id FROM conversations WHERE id = ?")
            .bind(conversation_id)
            .fetch_one(&mut *tx)
            .await?;
    if existing_session
        .as_deref()
        .is_some_and(|existing| existing != thread_id)
    {
        return Err(anyhow!(
            "Codex resumed a different thread than the canonical coordinator conversation"
        ));
    }

    let updated = sqlx::query(
        r#"
        UPDATE codex_coordinator_runtime
        SET codex_thread_id = ?, thread_state = 'ready', updated_at = ?
        WHERE conversation_id = ? AND thread_state = 'starting'
        "#,
    )
    .bind(thread_id)
    .bind(now)
    .bind(conversation_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(anyhow!(
            "Codex coordinator thread readiness did not match an active startup"
        ));
    }
    sqlx::query("UPDATE conversations SET session_id = ?, updated_at = ? WHERE id = ?")
        .bind(thread_id)
        .bind(Utc::now().to_rfc3339())
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    insert_event_tx(
        &mut tx,
        conversation_id,
        "thread_ready",
        Some(thread_id),
        Some("ready"),
        None,
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn require_thread_repair(
    pool: &SqlitePool,
    conversation_id: &str,
    error_class: &str,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let thread_id: Option<String> = sqlx::query_scalar(
        r#"
        UPDATE codex_coordinator_runtime
        SET thread_state = 'repair_required', last_terminal_status = 'failed',
            last_error_class = ?, updated_at = ?
        WHERE conversation_id = ?
        RETURNING codex_thread_id
        "#,
    )
    .bind(error_class)
    .bind(now)
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    insert_event(
        pool,
        conversation_id,
        "repair_required",
        thread_id.as_deref(),
        Some("repair_required"),
        Some(error_class),
        now,
    )
    .await?;
    Ok(())
}

pub async fn record_turn_terminal(
    pool: &SqlitePool,
    conversation_id: &str,
    thread_id: &str,
    status: &str,
    duration_ms: u64,
    tool_call_count: i32,
    error_class: Option<&str>,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let thread_state = if status == "failed" {
        "repair_required"
    } else {
        "ready"
    };
    let updated = sqlx::query(
        r#"
        UPDATE codex_coordinator_runtime
        SET thread_state = ?, last_completed_at = ?,
            last_success_at = CASE WHEN ? = 'completed' THEN ? ELSE last_success_at END,
            last_turn_duration_ms = ?, last_tool_call_count = ?,
            last_terminal_status = ?, last_error_class = ?, updated_at = ?
        WHERE conversation_id = ? AND codex_thread_id = ?
        "#,
    )
    .bind(thread_state)
    .bind(now)
    .bind(status)
    .bind(now)
    .bind(i64::try_from(duration_ms).unwrap_or(i64::MAX))
    .bind(tool_call_count)
    .bind(status)
    .bind(error_class)
    .bind(now)
    .bind(conversation_id)
    .bind(thread_id)
    .execute(pool)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(anyhow!(
            "Codex terminal state did not match the active coordinator thread"
        ));
    }
    insert_event(
        pool,
        conversation_id,
        "turn_terminal",
        Some(thread_id),
        Some(status),
        error_class,
        now,
    )
    .await?;
    Ok(())
}

pub async fn repair_thread(
    pool: &SqlitePool,
    conversation_id: &str,
    user_id: &str,
) -> Result<CodexCoordinatorThreadRepair> {
    require_alex(user_id)?;
    let conversation = conversations::get_conversation(pool, conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("Codex coordinator conversation not found"))?;
    validate_singleton(&conversation)?;
    let state = runtime_state(pool, conversation_id).await?;
    let repairable = state
        .as_ref()
        .is_some_and(|state| state.thread_state != "ready")
        || (state.is_none() && conversation.session_id.is_some());
    if !repairable {
        return Err(anyhow!(
            "The Codex coordinator does not have a visible thread repair incident"
        ));
    }

    let retired_thread_id = state
        .as_ref()
        .and_then(|state| state.codex_thread_id.clone())
        .or(conversation.session_id.clone());
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE conversations SET session_id = NULL, updated_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM codex_coordinator_runtime WHERE conversation_id = ?")
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    insert_event_tx(
        &mut tx,
        conversation_id,
        "thread_repair_approved",
        retired_thread_id.as_deref(),
        Some("reset"),
        None,
        now,
    )
    .await?;
    tx.commit().await?;

    Ok(CodexCoordinatorThreadRepair {
        retired_thread_id,
        rehydrate_required: true,
    })
}

async fn insert_event(
    pool: &SqlitePool,
    conversation_id: &str,
    event_type: &str,
    thread_id: Option<&str>,
    status: Option<&str>,
    detail: Option<&str>,
    created_at: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO codex_coordinator_events
            (id, conversation_id, event_type, thread_id, status, detail, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(conversation_id)
    .bind(event_type)
    .bind(thread_id)
    .bind(status)
    .bind(detail)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_event_tx(
    tx: &mut Transaction<'_, Sqlite>,
    conversation_id: &str,
    event_type: &str,
    thread_id: Option<&str>,
    status: Option<&str>,
    detail: Option<&str>,
    created_at: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO codex_coordinator_events
            (id, conversation_id, event_type, thread_id, status, detail, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(conversation_id)
    .bind(event_type)
    .bind(thread_id)
    .bind(status)
    .bind(detail)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool_without_coordinator_schema() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            r#"
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY, user_id TEXT NOT NULL, session_id TEXT,
                organization TEXT NOT NULL, agent TEXT, conversation_type TEXT,
                parent_conversation_id TEXT, conversation_role TEXT NOT NULL DEFAULT 'standard',
                child_conversation_count INTEGER NOT NULL DEFAULT 0,
                child_sort_order INTEGER, title TEXT NOT NULL, started_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'open', archived_at TEXT,
                router_ticket_id TEXT, router_organization TEXT,
                last_event_index INTEGER NOT NULL DEFAULT -1
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn test_pool() -> SqlitePool {
        let pool = test_pool_without_coordinator_schema().await;
        ensure_schema(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn legacy_fable_runtime_is_replaced_with_codex_only_state() {
        let pool = test_pool_without_coordinator_schema().await;
        sqlx::query("CREATE TABLE fable_codex_runtime (conversation_id TEXT PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE UNIQUE INDEX idx_fable_coordinator_singleton ON conversations(user_id) WHERE conversation_type = 'fable_coordinator'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            INSERT INTO conversations (
                id, user_id, session_id, organization, agent, conversation_type,
                parent_conversation_id, conversation_role, title, started_at,
                updated_at, status
            ) VALUES (
                'alex-coordinator', 'alex', 'legacy-thread', 'agentic-flowstate',
                'fable-coordinator', 'fable_coordinator', NULL,
                'multi_agent_parent', 'Alex', '2026-07-19T00:00:00Z',
                '2026-07-19T00:00:00Z', 'open'
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        ensure_schema(&pool).await.unwrap();

        let designation: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT agent, conversation_type, session_id FROM conversations WHERE id = 'alex-coordinator'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(designation.0.as_deref(), Some(CODEX_COORDINATOR_AGENT));
        assert_eq!(
            designation.1.as_deref(),
            Some(CODEX_COORDINATOR_CONVERSATION_TYPE)
        );
        assert!(designation.2.is_none());

        let legacy_objects: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'fable_%'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(legacy_objects, 0);

        let plan = prepare_thread(&pool, "alex-coordinator").await.unwrap();
        assert!(plan.resume_thread_id.is_none());
        assert!(plan.rehydrate_required);
    }

    #[tokio::test]
    async fn singleton_codex_thread_resume_is_durable() {
        let pool = test_pool().await;
        let conversation = ensure_singleton(&pool, ALEX_USER_ID).await.unwrap();
        let plan = prepare_thread(&pool, &conversation.id).await.unwrap();
        assert!(plan.resume_thread_id.is_none());
        assert!(plan.rehydrate_required);

        record_turn_started(&pool, &conversation.id, &plan, false)
            .await
            .unwrap();
        mark_thread_ready(&pool, &conversation.id, "thread-1")
            .await
            .unwrap();
        record_turn_terminal(
            &pool,
            &conversation.id,
            "thread-1",
            "completed",
            42,
            3,
            None,
        )
        .await
        .unwrap();

        let resumed = prepare_thread(&pool, &conversation.id).await.unwrap();
        assert_eq!(resumed.resume_thread_id.as_deref(), Some("thread-1"));
        assert!(!resumed.rehydrate_required);
    }

    #[tokio::test]
    async fn failed_thread_requires_explicit_repair_and_rehydration() {
        let pool = test_pool().await;
        let conversation = ensure_singleton(&pool, ALEX_USER_ID).await.unwrap();
        let plan = prepare_thread(&pool, &conversation.id).await.unwrap();
        record_turn_started(&pool, &conversation.id, &plan, false)
            .await
            .unwrap();
        mark_thread_ready(&pool, &conversation.id, "thread-1")
            .await
            .unwrap();
        record_turn_terminal(
            &pool,
            &conversation.id,
            "thread-1",
            "failed",
            12,
            0,
            Some("runtime_error"),
        )
        .await
        .unwrap();

        assert!(prepare_thread(&pool, &conversation.id).await.is_err());
        let repair = repair_thread(&pool, &conversation.id, ALEX_USER_ID)
            .await
            .unwrap();
        assert_eq!(repair.retired_thread_id.as_deref(), Some("thread-1"));
        assert!(repair.rehydrate_required);
        assert!(prepare_thread(&pool, &conversation.id)
            .await
            .unwrap()
            .resume_thread_id
            .is_none());
    }

    #[tokio::test]
    async fn runtime_assignment_is_exclusive_to_the_designated_singleton() {
        let pool = test_pool().await;
        let coordinator = ensure_singleton(&pool, ALEX_USER_ID).await.unwrap();
        validate_runtime_assignment(&coordinator, true).unwrap();
        assert!(validate_runtime_assignment(&coordinator, false).is_err());

        let mut worker = coordinator.clone();
        worker.id = "worker".to_string();
        worker.agent = Some("full-access".to_string());
        worker.conversation_type = Some("general".to_string());
        worker.conversation_role = "sub_agent".to_string();
        worker.parent_conversation_id = Some(coordinator.id);
        validate_runtime_assignment(&worker, false).unwrap();
        assert!(validate_runtime_assignment(&worker, true).is_err());
    }
}
