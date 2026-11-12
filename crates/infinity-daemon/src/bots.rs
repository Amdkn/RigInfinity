//! Bots — the coworkers surface (D-N051 F8, pas 7).
//!
//! Upstream (`CopilotKit/OpenBot` gel `2b5b14f`, `CopilotKit/openmuse`
//! gel `a4944f4`, `CopilotKit/OpenDots` gel `d4214af`,
//! `CopilotKit/OpenDuet` gel `f52288f`): four bots, one function —
//! an embodied coworker (workspace + browser + terminal). The
//! upstream itself repositions OpenDots and OpenDuet as *skins* of
//! OpenBot (commits `46eaf96`, `093e3b0`, post-gel), and openmuse
//! lists OpenBot as an unconfigured integration adapter
//! (`workspace.ts:357`, `openbotConfigured: false`).
//!
//! Native rule (D3): ONE native surface — a platform (OpenBot
//! reference) + 3 skins (tenants), not 4 apps. A skin is a tenant
//! configuration (agents / brand / channels / knowledge / model /
//! skills — the 6-YAML pattern of OpenDuet, generalised) + trait
//! functions in the native code, not a separate application.
//!
//! Native rules applied here:
//! - D11: ONE computer-service, `agent-computer` on `:4100`, header
//!   `x-openbot-bot-id` kept verbatim (already shared by openbot and
//!   OpenDots, `computer-service.ts:82`). `holder: bot | human` is
//!   the takeover rule: the same browser session, taken over by the
//!   human and handed back.
//! - D5: routines survive as job-loop entries (proof by receipt);
//!   `cron-parser` is abandoned, the 15-minute minimum is a job-loop
//!   policy, responsibilities are event jobs. There is no cron field
//!   anywhere in this module — `TaskSpec` denies unknown fields.
//! - D6: Intelligence is replaced by the gateway `:31415` +
//!   Shared Brain. No `CPK_INTELLIGENCE_API_KEY`, no
//!   `CPK_TELEMETRY_ID`, no provider key in native code. A coworker
//!   survives a restart without Intelligence: its thread is persisted
//!   in the Shared Brain and re-read after restart.
//! - D4: telemetry zero. The 5 triggers are abandoned or confirmed
//!   absent; this module emits nothing (`telemetry_emitters()` is
//!   empty). The SDK sink, product/desktop/setup telemetry and the
//!   OTEL export are not ported.
//! - D2: the OpenDuet vendu (openbot 0.2.0, 2 336 files) is
//!   ABANDONED with proof — only its identity (6 YAML + evals as
//!   fixtures) is ported, on the 0.3.3 gel. No 0.2.0 file survives
//!   in the native tree (H-P7-8, verified by the pas harness).
//! - D14: the 16 `agent-*` shims are abandoned (AG-UI acceptance is
//!   the platform: any AG-UI agent arrives as a coworker); Spire /
//!   SPIFFE, Postgres, desktops / mobiles / extensions as modules
//!   are abandoned — desktops, mobiles and the extension are
//!   surfaces, re-pointed, not daemons.
//! - D9: NAMED DEPENDENCY — the pas 6 functions (RLM, `/refine`,
//!   Continual Harness) are absent from the base `655b2a4`
//!   (`prime_agent` grep = 0, verified before this port). The
//!   `/refine` path is CONDITIONAL on the merge of `9050a1f`
//!   (`refine_available()` is false until then); the platform, the
//!   3 skins and the computer-service are unconditional.
//!
//! This module is the native mirror of the four adapters
//! (`aspace-<bot>-adapter`, TypeScript, stdlib only): same contract,
//! same function names, Rust types.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::BoxError;
use crate::job::{JobLoop, TaskKind, TaskSpec};
use crate::memory::SharedBrainMemory;
use crate::provider::{GATEWAY_URL, MODELPOOL_CHAIN};

/// The computer-service port (D11) — kept, not chosen.
pub const COMPUTER_PORT: u16 = 4100;

/// The computer-service header (D11) — kept verbatim.
pub const COMPUTER_HEADER: &str = "x-openbot-bot-id";

/// The browser surface port (OpenDots `:4311`) — a surface of the
/// computer-service, re-pointed, not a daemon.
pub const BROWSER_PORT: u16 = 4311;

/// Minimum routine interval (D5) — a job-loop policy, not cron syntax.
pub const MINIMUM_INTERVAL_MS: u64 = 15 * 60 * 1000;

/// The pas 6 native port, merged at F8 pas 8 (`9050a1f`, from
/// `ryan/d-n052-r2-native` into `aspace-v5-native`). The `/refine`
/// path is available — proved by behaviour, not by a flipped
/// constant (R-J10, D-N051 F8 pas 8).
pub const PAS6_NATIVE_COMMIT: &str = "9050a1f";

/// Keys that must never appear in native code (D6).
pub const FORBIDDEN_KEYS: &[&str] = &[
    "CPK_INTELLIGENCE_API_KEY",
    "CPK_TELEMETRY_ID",
    "OPENAI_API_KEY",
];

/// A skin is a tenant of the platform (D3), not an application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkinKind {
    Platform,
    Skin,
}

/// One skin registration (D10): id, name, capabilities, configured
/// state — the generalised openmuse → openbot adapter pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skin {
    pub id: String,
    pub name: String,
    pub kind: SkinKind,
    pub capabilities: Vec<String>,
    #[serde(rename = "trait")]
    pub trait_desc: String,
    pub configured: bool,
}

impl Skin {
    pub fn platform() -> Self {
        Self {
            id: "openbot".to_owned(),
            name: "OpenBot".to_owned(),
            kind: SkinKind::Platform,
            capabilities: vec![
                "ag_ui_acceptance".to_owned(),
                "computer_service_4100".to_owned(),
                "bot_workspace".to_owned(),
                "approval_boundary".to_owned(),
            ],
            trait_desc: "plateforme : accueille n'importe quel agent AG-UI comme coworker avec son ordinateur"
                .to_owned(),
            configured: true,
        }
    }

    pub fn opendots() -> Self {
        Self {
            id: "opendots".to_owned(),
            name: "OpenDots".to_owned(),
            kind: SkinKind::Skin,
            capabilities: vec![
                "always_on".to_owned(),
                "multi_channel".to_owned(),
                "review_before_save".to_owned(),
            ],
            trait_desc:
                "always-on : coworkers persistants, texte / appels / Slack, revue avant sauvegarde"
                    .to_owned(),
            configured: false,
        }
    }

    pub fn openduet() -> Self {
        Self {
            id: "openduet".to_owned(),
            name: "OpenDuet".to_owned(),
            kind: SkinKind::Skin,
            capabilities: vec![
                "proactive_brief".to_owned(),
                "human_in_loop".to_owned(),
                "task_approval".to_owned(),
            ],
            trait_desc: "proactif humain-dans-la-boucle : morning brief, Tasks, check-ins, approbations avec reprise"
                .to_owned(),
            configured: false,
        }
    }

    pub fn openmuse() -> Self {
        Self {
            id: "openmuse".to_owned(),
            name: "OpenMuse".to_owned(),
            kind: SkinKind::Skin,
            capabilities: vec![
                "personal_agent".to_owned(),
                "browser_takeover".to_owned(),
            ],
            trait_desc: "agent personnel : travail qui continue, reprise en main de la même session navigateur"
                .to_owned(),
            configured: false,
        }
    }
}

/// The three skins of the platform (D3).
pub fn all_skins() -> Vec<Skin> {
    vec![Skin::opendots(), Skin::openduet(), Skin::openmuse()]
}

/// The platform + its skins: one surface, four registrations.
#[derive(Debug, Default)]
pub struct Platform {
    skins: HashMap<String, Skin>,
}

impl Platform {
    pub fn new() -> Self {
        let mut platform = Self {
            skins: HashMap::new(),
        };
        platform.register(Skin::platform());
        platform
    }

    /// Register a skin on the platform (D10) — the same pattern for
    /// the 3 skins. `openbotConfigured` becomes `platformConfigured`.
    pub fn register(&mut self, mut skin: Skin) -> &Skin {
        skin.configured = true;
        let id = skin.id.clone();
        self.skins.insert(id.clone(), skin);
        self.skins.get(&id).expect("skin just registered")
    }

    pub fn get(&self, id: &str) -> Option<&Skin> {
        self.skins.get(id)
    }

    pub fn list(&self) -> Vec<&Skin> {
        let mut skins: Vec<&Skin> = self.skins.values().collect();
        skins.sort_by_key(|s| s.id.clone());
        skins
    }

    pub fn platform_configured(&self) -> bool {
        self.skins.contains_key("openbot") && self.skins.values().all(|s| s.configured)
    }

    /// The trait of a skin, proved by behaviour in the proof tests
    /// (H-P7-4): always-on, human-in-the-loop, personal agent.
    pub fn trait_of(&self, id: &str) -> Option<&str> {
        self.skins.get(id).map(|s| s.trait_desc.as_str())
    }
}

/// Who holds the computer (D11): the bot, or the human who took over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComputerHolder {
    Bot,
    Human,
}

impl ComputerHolder {
    pub fn parse(holder: &str) -> Result<Self, BoxError> {
        match holder {
            "bot" => Ok(Self::Bot),
            "human" => Ok(Self::Human),
            other => Err(format!("holder inconnu: {other} (attendu bot | human)").into()),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Human => "human",
        }
    }
}

/// The single native computer-service (D11).
#[derive(Debug, Clone)]
pub struct ComputerService {
    pub host: String,
}

impl ComputerService {
    pub fn new() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}", self.host, COMPUTER_PORT)
    }

    pub fn browser_url(&self) -> String {
        format!("http://{}:{}", self.host, BROWSER_PORT)
    }

    /// The header is emitted verbatim on every computer call.
    pub fn headers_for(&self, bot_id: &str) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(COMPUTER_HEADER.to_owned(), bot_id.to_owned());
        headers
    }
}

impl Default for ComputerService {
    fn default() -> Self {
        Self::new()
    }
}

/// One browser session on the computer-service: the session survives
/// the takeover (H-P7-4, H-P7-5) — the same session id, held by the
/// bot, then by the human, then by the bot again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub session_id: String,
    pub bot_id: String,
    pub holder: ComputerHolder,
}

impl BrowserSession {
    pub fn new(session_id: &str, bot_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            bot_id: bot_id.to_owned(),
            holder: ComputerHolder::Bot,
        }
    }

    /// Take over: the human takes the same session; handing back
    /// returns it to the bot. The session id never changes.
    pub fn takeover(&mut self) {
        self.holder = match self.holder {
            ComputerHolder::Bot => ComputerHolder::Human,
            ComputerHolder::Human => ComputerHolder::Bot,
        };
    }
}

/// Telemetry emitters in the native surface (D4): none.
/// The 5 triggers (SDK sink, product telemetry, desktop telemetry,
/// setup telemetry, OTEL export) are abandoned or confirmed absent.
pub fn telemetry_emitters() -> Vec<&'static str> {
    Vec::new()
}

/// True when a text carries a forbidden key (D6). Used by the proof
/// to scan the native tree: no CPK key, no provider key.
pub fn contains_forbidden_key(text: &str) -> bool {
    FORBIDDEN_KEYS.iter().any(|k| text.contains(k))
}

/// The `/refine` path, available since the pas 6 merge (D9, F8
/// pas 8). Availability is PROVED BY BEHAVIOUR (R-J10), never by a
/// flipped constant: this runs the merged `prime_agent_refine`
/// machinery for real — plan a refinement, apply it to an
/// in-memory harness state, read the entry back — and returns true
/// only when the whole round-trip succeeds. If the module were
/// absent or broken, the code would not compile or the probe would
/// fail; the platform, the 3 skins and the computer-service never
/// waited for it, and now they can call it.
pub fn refine_available() -> bool {
    use crate::prime_agent_refine::{
        HarnessState, RefineOptions, RefinementAction, RefinementEdit, RefinementKind,
        RefinementProposal, apply_refinement_plan, plan_refinement,
    };
    let mut state = HarnessState::default();
    let options = RefineOptions::default();
    let plan = match plan_refinement(&[], &options, &|_| {
        Ok(RefinementProposal {
            summary: "refine availability probe".to_owned(),
            rationale: "prove /refine is merged and working (R-J10)".to_owned(),
            expected_outcome: "one harness entry created".to_owned(),
            edits: vec![RefinementEdit {
                action: RefinementAction::Create,
                kind: RefinementKind::Prompt,
                id: None,
                title: "probe".to_owned(),
                content: "probe".to_owned(),
            }],
        })
    }) {
        Ok(plan) => plan,
        Err(_) => return false,
    };
    let result = apply_refinement_plan(&mut state, plan, &options, None);
    result.applied_edits.iter().any(|edit| edit.applied) && !state.is_empty()
}

pub fn refine_unavailable_reason() -> String {
    if refine_available() {
        String::new()
    } else {
        format!(
            "pas 6 non mergé : le portage natif {PAS6_NATIVE_COMMIT} vit sur ryan/d-n052-r2-native ; /refine est conditionnel à son merge (§D9)"
        )
    }
}

/// A routine as a job-loop entry (D5): the schedule becomes a
/// `TaskSpec`, the proof is a receipt. The interval floor is enforced
/// here; a cron expression never reaches the loop.
pub fn routine_to_job(
    name: &str,
    run_at: Option<u64>,
    interval_ms: u64,
    action: &str,
) -> Result<TaskSpec, BoxError> {
    if interval_ms < MINIMUM_INTERVAL_MS {
        return Err(format!(
            "intervalle refusé : minimum {MINIMUM_INTERVAL_MS} ms, reçu {interval_ms} ms"
        )
        .into());
    }
    Ok(TaskSpec {
        kind: TaskKind::Scheduled,
        name: name.to_owned(),
        payload: serde_json::json!({ "action": action, "interval_ms": interval_ms }),
        run_at,
        handler: "bots-routine".to_owned(),
    })
}

/// A responsibility (goal) as an event job (D5): it runs when asked,
/// or when an event wakes it — never on a cron.
pub fn event_job(name: &str, action: &str) -> TaskSpec {
    TaskSpec {
        kind: TaskKind::Adhoc,
        name: name.to_owned(),
        payload: serde_json::json!({ "action": action }),
        run_at: None,
        handler: "bots-event".to_owned(),
    }
}

/// Register the bots handlers on a job loop: routines and event jobs
/// execute inside the single daemon's loop (D5, H-P7-5).
pub fn register_bots_handlers(job: &mut JobLoop) {
    job.register_handler(
        "bots-routine",
        std::sync::Arc::new(|payload| payload["action"].as_str().unwrap_or("routine").to_owned()),
    );
    job.register_handler(
        "bots-event",
        std::sync::Arc::new(|payload| payload["action"].as_str().unwrap_or("event").to_owned()),
    );
}

/// One coworker (D6): a persistent thread without Intelligence.
/// The thread lives in the Shared Brain JSONL; a new `Coworker` on
/// the same brain path re-reads it after a restart — the coworker
/// survives without any hosted service and without any key.
#[derive(Debug)]
pub struct Coworker {
    pub chat_id: String,
    pub skin_id: String,
    brain: SharedBrainMemory,
}

impl Coworker {
    pub fn new(chat_id: &str, skin_id: &str, brain_path: impl AsRef<Path>) -> Self {
        Self {
            chat_id: chat_id.to_owned(),
            skin_id: skin_id.to_owned(),
            brain: SharedBrainMemory::new(brain_path),
        }
    }

    /// Persist one turn of the thread in the Shared Brain.
    pub fn say(&self, role: &str, content: &str) -> Result<(), BoxError> {
        self.brain.append(
            "coworker-turn",
            content,
            serde_json::json!({ "chat_id": self.chat_id, "skin": self.skin_id, "role": role }),
        )?;
        Ok(())
    }

    /// Re-read the thread after a restart (H-P7-6).
    pub fn thread(&self) -> Result<Vec<String>, BoxError> {
        let mut turns = Vec::new();
        for entry in self.brain.read_all()? {
            if entry.kind == "coworker-turn"
                && entry.meta.get("chat_id").and_then(|v| v.as_str()) == Some(self.chat_id.as_str())
            {
                turns.push(entry.content);
            }
        }
        Ok(turns)
    }

    /// The model route of the coworker: the gateway only, the
    /// modelPool chain, no key (D6, patron pas 5 D9).
    pub fn gateway(&self) -> (&'static str, &[&str]) {
        (GATEWAY_URL, MODELPOOL_CHAIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("riginfinity-bots-{tag}-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn platform_registers_three_skins() {
        let mut platform = Platform::new();
        assert!(platform.platform_configured());
        for skin in all_skins() {
            platform.register(skin);
        }
        assert_eq!(platform.list().len(), 4);
        assert!(platform.platform_configured());
        assert_eq!(platform.get("opendots").expect("dot").kind, SkinKind::Skin);
        assert_eq!(platform.get("openduet").expect("duet").kind, SkinKind::Skin);
        assert_eq!(platform.get("openmuse").expect("muse").kind, SkinKind::Skin);
        assert!(
            platform
                .trait_of("opendots")
                .expect("trait")
                .contains("always-on")
        );
        assert!(
            platform
                .trait_of("openduet")
                .expect("trait")
                .contains("humain-dans-la-boucle")
        );
        assert!(
            platform
                .trait_of("openmuse")
                .expect("trait")
                .contains("agent personnel")
        );
    }

    #[test]
    fn computer_service_port_and_header_verbatim() {
        let service = ComputerService::new();
        assert_eq!(COMPUTER_PORT, 4100);
        assert_eq!(COMPUTER_HEADER, "x-openbot-bot-id");
        assert_eq!(service.url(), "http://127.0.0.1:4100");
        assert_eq!(service.browser_url(), "http://127.0.0.1:4311");
        let headers = service.headers_for("bot-7");
        assert_eq!(
            headers.get(COMPUTER_HEADER).map(String::as_str),
            Some("bot-7")
        );
        assert_eq!(
            ComputerHolder::parse("bot").expect("bot"),
            ComputerHolder::Bot
        );
        assert_eq!(
            ComputerHolder::parse("human").expect("human"),
            ComputerHolder::Human
        );
        assert!(ComputerHolder::parse("autre").is_err());
    }

    #[test]
    fn browser_session_survives_takeover() {
        let mut session = BrowserSession::new("sess-1", "bot-7");
        assert_eq!(session.holder, ComputerHolder::Bot);
        session.takeover();
        assert_eq!(session.holder, ComputerHolder::Human);
        assert_eq!(session.session_id, "sess-1");
        session.takeover();
        assert_eq!(session.holder, ComputerHolder::Bot);
        assert_eq!(session.session_id, "sess-1");
    }

    #[test]
    fn telemetry_zero_and_no_forbidden_key() {
        assert!(telemetry_emitters().is_empty());
        assert!(!contains_forbidden_key(GATEWAY_URL));
        assert!(!contains_forbidden_key("http://127.0.0.1:4100"));
        assert!(contains_forbidden_key("CPK_INTELLIGENCE_API_KEY=x"));
        assert!(contains_forbidden_key("key CPK_TELEMETRY_ID here"));
    }

    #[test]
    fn refine_is_available_after_pas6_merge() {
        // R-J10 (F8 pas 8): merged — proved by behaviour, not by a
        // flipped constant. The probe inside `refine_available()`
        // plans and applies a real refinement; here the merged
        // module is exercised directly as well.
        assert!(refine_available());
        assert!(refine_unavailable_reason().is_empty());
        assert_eq!(PAS6_NATIVE_COMMIT, "9050a1f");
        use crate::prime_agent_refine::{
            HarnessState, RefineOptions, RefinementAction, RefinementEdit, RefinementKind,
            RefinementProposal, apply_refinement_plan, plan_refinement,
        };
        let mut state = HarnessState::default();
        let options = RefineOptions::default();
        let plan = plan_refinement(&[], &options, &|_| {
            Ok(RefinementProposal {
                summary: "pas 8 unit proof".to_owned(),
                rationale: "direct plan + apply proof".to_owned(),
                expected_outcome: "one entry, readable back".to_owned(),
                edits: vec![RefinementEdit {
                    action: RefinementAction::Create,
                    kind: RefinementKind::Memory,
                    id: None,
                    title: "unit-proof".to_owned(),
                    content: "unit-proof".to_owned(),
                }],
            })
        })
        .expect("plan a refinement");
        let result = apply_refinement_plan(&mut state, plan, &options, None);
        assert!(result.applied_edits.iter().any(|edit| edit.applied));
        assert_eq!(
            state
                .get("unit-proof")
                .expect("entry readable back")
                .content,
            "unit-proof"
        );
    }

    #[test]
    fn routine_refuses_sub_floor_interval() {
        let err =
            routine_to_job("brief", None, 60_000, "brief").expect_err("intervalle < 15 min refusé");
        assert!(err.to_string().contains("minimum"));
        let spec = routine_to_job("brief", Some(2000), 3_600_000, "brief").expect("routine");
        assert_eq!(spec.kind, TaskKind::Scheduled);
        assert_eq!(spec.handler, "bots-routine");
    }

    #[test]
    fn routine_executes_in_the_job_loop_with_receipt() {
        let dir = tmp_dir("routine");
        let mut job = JobLoop::new(dir.join("ledger.jsonl"));
        register_bots_handlers(&mut job);
        let spec = routine_to_job("morning-brief", Some(100), 3_600_000, "morning brief")
            .expect("routine");
        let task = job.create_task(spec).expect("create task");
        let receipts = job.run_due(100).expect("run due");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].task_id, task.task_id);
        assert_eq!(receipts[0].result, "morning brief");
        assert_eq!(job.read_receipts().expect("receipts").len(), 1);
        let event = job
            .create_task(event_job("goal-check", "check goal"))
            .expect("event job");
        let receipt = job.run_task(&event.task_id, 200).expect("run event");
        assert_eq!(receipt.result, "check goal");
    }

    #[test]
    fn coworker_survives_restart_without_intelligence() {
        let dir = tmp_dir("coworker");
        let brain = dir.join("memory.jsonl");
        {
            let coworker = Coworker::new("chat-1", "openduet", &brain);
            coworker
                .say("user", "prépare le brief du matin")
                .expect("say");
            coworker.say("assistant", "brief prêt").expect("say");
            assert_eq!(coworker.gateway().0, GATEWAY_URL);
            assert_eq!(coworker.gateway().1, MODELPOOL_CHAIN);
            assert_eq!(coworker.thread().expect("thread").len(), 2);
        }
        // Restart: a new instance on the same brain path re-reads it.
        let restarted = Coworker::new("chat-1", "openduet", &brain);
        let thread = restarted.thread().expect("thread after restart");
        assert_eq!(thread.len(), 2);
        assert!(thread[0].contains("brief du matin"));
        assert!(thread[1].contains("brief prêt"));
        // Another chat is not mixed in.
        assert_eq!(
            Coworker::new("chat-2", "openmuse", &brain)
                .thread()
                .expect("other")
                .len(),
            0
        );
    }
}
