use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};
use ticketing_system::{conversations, Conversation};

use crate::agents::claude_code::{FABLE_EFFORT, FABLE_MCP_PROFILE, FABLE_MODEL};

pub const ALEX_USER_ID: &str = "alex";
pub const FABLE_CONVERSATION_TYPE: &str = "fable_coordinator";
pub const FABLE_AGENT: &str = "fable-coordinator";
pub const FABLE_PROMPT_VERSION: &str = "fable-coordinator/v1";
pub const FABLE_ORGANIZATION: &str = "agentic-flowstate";
pub const FABLE_TITLE: &str = "Alex";

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct FableRuntimeState {
    pub conversation_id: String,
    pub native_session_id: String,
    pub session_state: String,
    pub prompt_version: String,
    pub model: String,
    pub effort: String,
    pub auth_method: String,
    pub subscription_type: String,
    pub mcp_profile: String,
    pub rehydrate_required: bool,
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
pub struct FableSessionPlan {
    pub session_id: String,
    pub resume: bool,
    pub rehydrate_required: bool,
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS fable_coordinator_runtime (
            conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE RESTRICT,
            native_session_id TEXT NOT NULL UNIQUE,
            session_state TEXT NOT NULL CHECK (
                session_state IN (
                    'provisioning', 'ready', 'recovery_required', 'recovery_approved'
                )
            ),
            prompt_version TEXT NOT NULL,
            model TEXT NOT NULL,
            effort TEXT NOT NULL,
            auth_method TEXT NOT NULL,
            subscription_type TEXT NOT NULL,
            mcp_profile TEXT NOT NULL,
            rehydrate_required INTEGER NOT NULL DEFAULT 0 CHECK (rehydrate_required IN (0, 1)),
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
    .execute(pool)
    .await
    .context("create Fable coordinator runtime table")?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS fable_coordinator_events (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE RESTRICT,
            event_type TEXT NOT NULL,
            session_id TEXT,
            status TEXT,
            detail TEXT,
            created_at INTEGER NOT NULL
        )
        "#,
    )
    .execute(pool)
    .await
    .context("create Fable coordinator event table")?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_fable_coordinator_events_conversation_created ON fable_coordinator_events(conversation_id, created_at DESC)",
    )
    .execute(pool)
    .await
    .context("index Fable coordinator events")?;
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS idx_fable_coordinator_singleton
        ON conversations(user_id)
        WHERE conversation_type = 'fable_coordinator'
        "#,
    )
    .execute(pool)
    .await
    .context("enforce one Fable coordinator conversation per user")?;
    Ok(())
}

pub fn require_alex(user_id: &str) -> Result<()> {
    if user_id == ALEX_USER_ID {
        Ok(())
    } else {
        Err(anyhow!("The permanent Fable coordinator is owned by Alex"))
    }
}

pub fn is_fable_conversation(conversation: &Conversation) -> bool {
    conversation.conversation_type.as_deref() == Some(FABLE_CONVERSATION_TYPE)
}

pub fn validate_runtime_assignment(
    conversation: &Conversation,
    fable_coordinator_agent: bool,
) -> Result<()> {
    let has_fable_type = conversation.conversation_type.as_deref() == Some(FABLE_CONVERSATION_TYPE);
    let has_fable_agent = conversation.agent.as_deref() == Some(FABLE_AGENT);
    let designated = has_fable_type && has_fable_agent;

    if fable_coordinator_agent {
        if !designated {
            return Err(anyhow!(
                "The Fable coordinator agent is reserved for Alex's permanent coordinator"
            ));
        }
        return validate_singleton(conversation);
    }

    if has_fable_type || has_fable_agent {
        return Err(anyhow!(
            "Alex's permanent coordinator must run through the Fable coordinator agent"
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
    .bind(FABLE_ORGANIZATION)
    .bind(FABLE_AGENT)
    .bind(FABLE_CONVERSATION_TYPE)
    .bind(FABLE_TITLE)
    .bind(&now_iso)
    .bind(&now_iso)
    .execute(pool)
    .await
    .context("create permanent Fable coordinator conversation")?;

    let conversation_id: String = sqlx::query_scalar(
        r#"
        SELECT id
        FROM conversations
        WHERE user_id = ? AND conversation_type = ?
        "#,
    )
    .bind(user_id)
    .bind(FABLE_CONVERSATION_TYPE)
    .fetch_one(pool)
    .await
    .context("load permanent Fable coordinator conversation id")?;
    let conversation = conversations::get_conversation(pool, &conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("Permanent Fable coordinator disappeared after creation"))?;
    validate_singleton(&conversation)?;
    Ok(conversation)
}

fn validate_singleton(conversation: &Conversation) -> Result<()> {
    if conversation.user_id != ALEX_USER_ID
        || conversation.agent.as_deref() != Some(FABLE_AGENT)
        || conversation.conversation_type.as_deref() != Some(FABLE_CONVERSATION_TYPE)
        || conversation.parent_conversation_id.is_some()
        || conversation.conversation_role != "multi_agent_parent"
        || conversation.status != "open"
        || conversation.archived_at.is_some()
    {
        return Err(anyhow!(
            "Permanent Fable coordinator {} has invalid durable designation; explicit repair is required",
            conversation.id
        ));
    }
    Ok(())
}

pub async fn runtime_state(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<FableRuntimeState>> {
    sqlx::query_as::<_, FableRuntimeState>(
        r#"
        SELECT conversation_id, native_session_id, session_state, prompt_version,
               model, effort, auth_method, subscription_type, mcp_profile,
               rehydrate_required, last_started_at, last_completed_at,
               last_success_at, last_turn_duration_ms, last_tool_call_count,
               last_terminal_status, last_error_class, last_wake_at,
               created_at, updated_at
        FROM fable_coordinator_runtime
        WHERE conversation_id = ?
        "#,
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
    .context("load Fable coordinator runtime state")
}

pub async fn prepare_session(pool: &SqlitePool, conversation_id: &str) -> Result<FableSessionPlan> {
    ensure_schema(pool).await?;
    let conversation = conversations::get_conversation(pool, conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("Fable coordinator conversation not found"))?;
    validate_singleton(&conversation)?;

    if let Some(state) = runtime_state(pool, conversation_id).await? {
        if conversation.session_id.as_deref() != Some(state.native_session_id.as_str()) {
            return Err(anyhow!(
                "Fable native session metadata diverged from the canonical conversation; explicit repair is required"
            ));
        }
        return match state.session_state.as_str() {
            "ready" => Ok(FableSessionPlan {
                session_id: state.native_session_id,
                resume: true,
                rehydrate_required: state.rehydrate_required,
            }),
            "recovery_approved" => {
                let session_id = state.native_session_id;
                let now = Utc::now().timestamp();
                let mut tx = pool.begin().await?;
                let claimed = sqlx::query(
                    r#"
                    UPDATE fable_coordinator_runtime
                    SET session_state = 'provisioning', updated_at = ?
                    WHERE conversation_id = ? AND native_session_id = ?
                      AND session_state = 'recovery_approved'
                    "#,
                )
                .bind(now)
                .bind(conversation_id)
                .bind(&session_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if claimed != 1 {
                    return Err(anyhow!(
                        "Approved Fable recovery raced with another coordinator turn"
                    ));
                }
                insert_event_tx(
                    &mut tx,
                    conversation_id,
                    "session_reprovisioning",
                    Some(&session_id),
                    Some("provisioning"),
                    None,
                    now,
                )
                .await?;
                tx.commit().await?;
                Ok(FableSessionPlan {
                    session_id,
                    resume: false,
                    rehydrate_required: true,
                })
            }
            "provisioning" => Err(anyhow!(
                "Fable native session provisioning was interrupted; explicit audited recovery is required"
            )),
            "recovery_required" => Err(anyhow!(
                "Fable native session recovery is required before another coordinator turn"
            )),
            other => Err(anyhow!("Unsupported Fable session state: {other}")),
        };
    }

    if conversation.session_id.is_some() {
        return Err(anyhow!(
            "Fable conversation has an untracked native session; explicit audited recovery is required"
        ));
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE conversations SET session_id = ?, updated_at = ? WHERE id = ? AND session_id IS NULL",
    )
    .bind(&session_id)
    .bind(Utc::now().to_rfc3339())
    .bind(conversation_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated != 1 {
        return Err(anyhow!(
            "Fable native session assignment raced with another coordinator turn"
        ));
    }
    sqlx::query(
        r#"
        INSERT INTO fable_coordinator_runtime (
            conversation_id, native_session_id, session_state, prompt_version,
            model, effort, auth_method, subscription_type, mcp_profile,
            rehydrate_required, created_at, updated_at
        ) VALUES (?, ?, 'provisioning', ?, ?, ?, 'claude.ai', 'max', ?, 0, ?, ?)
        "#,
    )
    .bind(conversation_id)
    .bind(&session_id)
    .bind(FABLE_PROMPT_VERSION)
    .bind(FABLE_MODEL)
    .bind(FABLE_EFFORT)
    .bind(FABLE_MCP_PROFILE)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    insert_event_tx(
        &mut tx,
        conversation_id,
        "session_assigned",
        Some(&session_id),
        Some("provisioning"),
        None,
        now,
    )
    .await?;
    tx.commit().await?;

    Ok(FableSessionPlan {
        session_id,
        resume: false,
        rehydrate_required: false,
    })
}

pub async fn mark_session_ready(
    pool: &SqlitePool,
    conversation_id: &str,
    session_id: &str,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let result = sqlx::query(
        r#"
        UPDATE fable_coordinator_runtime
        SET session_state = 'ready', updated_at = ?
        WHERE conversation_id = ? AND native_session_id = ?
        "#,
    )
    .bind(now)
    .bind(conversation_id)
    .bind(session_id)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!(
            "Fable session init did not match durable runtime state"
        ));
    }
    insert_event(
        pool,
        conversation_id,
        "session_ready",
        Some(session_id),
        Some("ready"),
        None,
    )
    .await
}

pub async fn record_turn_started(
    pool: &SqlitePool,
    conversation_id: &str,
    session_id: &str,
    is_wake: bool,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let result = sqlx::query(
        r#"
        UPDATE fable_coordinator_runtime
        SET last_started_at = ?, last_terminal_status = 'running',
            last_error_class = NULL,
            last_wake_at = CASE WHEN ? THEN ? ELSE last_wake_at END,
            updated_at = ?
        WHERE conversation_id = ? AND native_session_id = ?
        "#,
    )
    .bind(now)
    .bind(is_wake)
    .bind(now)
    .bind(now)
    .bind(conversation_id)
    .bind(session_id)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!(
            "Fable turn start did not match durable session state"
        ));
    }
    insert_event(
        pool,
        conversation_id,
        if is_wake {
            "wake_started"
        } else {
            "turn_started"
        },
        Some(session_id),
        Some("running"),
        None,
    )
    .await
}

pub async fn record_turn_terminal(
    pool: &SqlitePool,
    conversation_id: &str,
    session_id: &str,
    status: &str,
    duration_ms: u64,
    tool_call_count: i32,
    error_class: Option<&str>,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let success = status == "completed";
    let result = sqlx::query(
        r#"
        UPDATE fable_coordinator_runtime
        SET last_completed_at = ?,
            last_success_at = CASE WHEN ? THEN ? ELSE last_success_at END,
            last_turn_duration_ms = ?, last_tool_call_count = ?,
            last_terminal_status = ?, last_error_class = ?,
            rehydrate_required = CASE WHEN ? THEN 0 ELSE rehydrate_required END,
            updated_at = ?
        WHERE conversation_id = ? AND native_session_id = ?
        "#,
    )
    .bind(now)
    .bind(success)
    .bind(now)
    .bind(i64::try_from(duration_ms).unwrap_or(i64::MAX))
    .bind(tool_call_count)
    .bind(status)
    .bind(error_class)
    .bind(success)
    .bind(now)
    .bind(conversation_id)
    .bind(session_id)
    .execute(pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(anyhow!(
            "Fable turn terminal update did not match durable session state"
        ));
    }
    insert_event(
        pool,
        conversation_id,
        "turn_terminal",
        Some(session_id),
        Some(status),
        error_class,
    )
    .await
}

pub async fn require_session_recovery(
    pool: &SqlitePool,
    conversation_id: &str,
    error_class: &str,
) -> Result<()> {
    let now = Utc::now().timestamp();
    let session_id: Option<String> = sqlx::query_scalar(
        r#"
        UPDATE fable_coordinator_runtime
        SET session_state = 'recovery_required', last_terminal_status = 'failed',
            last_error_class = ?, updated_at = ?
        WHERE conversation_id = ?
        RETURNING native_session_id
        "#,
    )
    .bind(error_class)
    .bind(now)
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;
    if session_id.is_none() {
        return Err(anyhow!(
            "Fable recovery incident did not match durable runtime state"
        ));
    }
    insert_event(
        pool,
        conversation_id,
        "recovery_required",
        session_id.as_deref(),
        Some("failed"),
        Some(error_class),
    )
    .await
}

pub async fn repair_session(
    pool: &SqlitePool,
    conversation_id: &str,
    actor: &str,
) -> Result<FableSessionPlan> {
    require_alex(actor)?;
    let conversation = conversations::get_conversation(pool, conversation_id, false)
        .await?
        .ok_or_else(|| anyhow!("Fable coordinator conversation not found"))?;
    validate_singleton(&conversation)?;
    let state = runtime_state(pool, conversation_id)
        .await?
        .ok_or_else(|| anyhow!("Fable runtime state does not exist"))?;
    if state.session_state != "recovery_required" {
        return Err(anyhow!(
            "Fable session repair is only allowed after a visible recovery incident"
        ));
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE conversations SET session_id = ?, updated_at = ? WHERE id = ?")
        .bind(&session_id)
        .bind(Utc::now().to_rfc3339())
        .bind(conversation_id)
        .execute(&mut *tx)
        .await?;
    let repaired = sqlx::query(
        r#"
        UPDATE fable_coordinator_runtime
        SET native_session_id = ?, session_state = 'recovery_approved',
            prompt_version = ?, model = ?, effort = ?, auth_method = 'claude.ai',
            subscription_type = 'max', mcp_profile = ?, rehydrate_required = 1,
            last_terminal_status = 'recovery_approved', last_error_class = NULL,
            updated_at = ?
        WHERE conversation_id = ?
        "#,
    )
    .bind(&session_id)
    .bind(FABLE_PROMPT_VERSION)
    .bind(FABLE_MODEL)
    .bind(FABLE_EFFORT)
    .bind(FABLE_MCP_PROFILE)
    .bind(now)
    .bind(conversation_id)
    .execute(&mut *tx)
    .await?;
    if repaired.rows_affected() != 1 {
        return Err(anyhow!(
            "Fable session repair did not match durable runtime state"
        ));
    }
    insert_event_tx(
        &mut tx,
        conversation_id,
        "session_repair_approved",
        Some(&session_id),
        Some("recovery_approved"),
        Some(actor),
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(FableSessionPlan {
        session_id,
        resume: false,
        rehydrate_required: true,
    })
}

async fn insert_event(
    pool: &SqlitePool,
    conversation_id: &str,
    event_type: &str,
    session_id: Option<&str>,
    status: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO fable_coordinator_events
            (id, conversation_id, event_type, session_id, status, detail, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(conversation_id)
    .bind(event_type)
    .bind(session_id)
    .bind(status)
    .bind(detail)
    .bind(Utc::now().timestamp())
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    conversation_id: &str,
    event_type: &str,
    session_id: Option<&str>,
    status: Option<&str>,
    detail: Option<&str>,
    created_at: i64,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO fable_coordinator_events
            (id, conversation_id, event_type, session_id, status, detail, created_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(conversation_id)
    .bind(event_type)
    .bind(session_id)
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

    async fn test_pool() -> SqlitePool {
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
                router_ticket_id TEXT, router_organization TEXT, last_event_index INTEGER NOT NULL DEFAULT -1
            )
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        ensure_schema(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn singleton_creation_and_session_resume_are_durable() {
        let pool = test_pool().await;
        let first = ensure_singleton(&pool, ALEX_USER_ID).await.unwrap();
        let second = ensure_singleton(&pool, ALEX_USER_ID).await.unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(first.agent.as_deref(), Some("fable-coordinator"));

        let plan = prepare_session(&pool, &first.id).await.unwrap();
        assert!(!plan.resume);
        mark_session_ready(&pool, &first.id, &plan.session_id)
            .await
            .unwrap();
        let resumed = prepare_session(&pool, &first.id).await.unwrap();
        assert!(resumed.resume);
        assert_eq!(resumed.session_id, plan.session_id);
    }

    #[tokio::test]
    async fn interrupted_provisioning_requires_explicit_recovery() {
        let pool = test_pool().await;
        let conversation = ensure_singleton(&pool, ALEX_USER_ID).await.unwrap();
        prepare_session(&pool, &conversation.id).await.unwrap();
        let error = prepare_session(&pool, &conversation.id)
            .await
            .expect_err("interrupted provisioning must not silently rotate");
        assert!(error.to_string().contains("audited recovery"));

        require_session_recovery(&pool, &conversation.id, "session_init_failed")
            .await
            .unwrap();
        let repaired = repair_session(&pool, &conversation.id, ALEX_USER_ID)
            .await
            .unwrap();
        assert!(!repaired.resume);
        assert!(repaired.rehydrate_required);
        let reprovisioned = prepare_session(&pool, &conversation.id).await.unwrap();
        assert_eq!(reprovisioned.session_id, repaired.session_id);
        assert!(!reprovisioned.resume);
        assert!(reprovisioned.rehydrate_required);
    }

    #[tokio::test]
    async fn non_alex_users_cannot_create_or_repair_coordinator() {
        let pool = test_pool().await;
        assert!(ensure_singleton(&pool, "someone-else").await.is_err());
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
