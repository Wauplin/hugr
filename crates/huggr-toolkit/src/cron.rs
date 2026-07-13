use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use chrono::{DateTime, Utc};
use croner::Cron;
use serde_json::json;
use tokio::sync::Mutex;

use crate::manifest::{AgentDefinition, CronJobConfig, LimitsConfig};
use crate::runtime::{RuntimeOptions, build_agent_with_options};

pub async fn serve_definition(
    def: AgentDefinition,
    options: RuntimeOptions,
    allow_uncapped: bool,
) -> i32 {
    if def.cron.is_empty() {
        eprintln!("error: no [cron.<name>] jobs configured");
        return 1;
    }
    if let Err(err) = validate_cost_caps(&def, allow_uncapped) {
        eprintln!("error: {err}");
        return 1;
    }
    let (agent, warnings) = match build_agent_with_options(&def, &options).await {
        Ok(result) => result,
        Err(err) => {
            eprintln!("error: {err}");
            return 1;
        }
    };
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut handles = Vec::new();
    for job in def.cron.clone() {
        let Some(schedule) = parse_schedule(&job).ok() else {
            eprintln!("error: invalid cron schedule for `{}`", job.name);
            return 1;
        };
        let agent = agent.clone();
        handles.push(tokio::spawn(async move {
            run_job_loop(agent, job, schedule).await;
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
    0
}

pub fn validate_cost_caps(def: &AgentDefinition, allow_uncapped: bool) -> Result<(), String> {
    if allow_uncapped {
        return Ok(());
    }
    for job in &def.cron {
        if effective_limits(&def.limits, &job.limits)
            .max_cost_micro_usd
            .is_none()
        {
            return Err(format!(
                "[cron.{}] has no max_cost_micro_usd; set one in [limits] or [cron.{}.limits], or pass --allow-uncapped",
                job.name, job.name
            ));
        }
    }
    Ok(())
}

pub fn next_fire_after(schedule: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let cron = parse_cron(schedule)?;
    cron.find_next_occurrence(&now, false)
        .map_err(|err| err.to_string())
}

fn parse_schedule(job: &CronJobConfig) -> Result<Cron, String> {
    parse_cron(&job.schedule)
}

fn parse_cron(schedule: &str) -> Result<Cron, String> {
    Cron::new(schedule)
        .with_seconds_optional()
        .parse()
        .map_err(|err| err.to_string())
}

async fn run_job_loop(agent: huggr_agent::Agent, job: CronJobConfig, schedule: Cron) {
    let running = Arc::new(AtomicBool::new(false));
    let recovered = if job.lineage == "chain" {
        recover_chain_parent(&agent, &job.name).await
    } else {
        None
    };
    if let Some(parent) = &recovered {
        eprintln!("cron `{}` resuming chain from trace {parent}", job.name);
    }
    let chain_parent = Arc::new(Mutex::new(recovered));
    loop {
        let now = Utc::now();
        let next = match schedule.find_next_occurrence(&now, false) {
            Ok(next) => next,
            Err(err) => {
                eprintln!("cron `{}` schedule error: {err}", job.name);
                return;
            }
        };
        let sleep = (next - now)
            .to_std()
            .unwrap_or_else(|_| std::time::Duration::from_secs(0));
        tokio::time::sleep(sleep).await;

        if running.swap(true, Ordering::SeqCst) {
            eprintln!("cron `{}` skipped overlapping fire at {next}", job.name);
            continue;
        }
        let mut agent_for_fire = agent.clone();
        agent_for_fire.limits = effective_limits(&agent.limits.clone().into(), &job.limits).into();
        let job_for_fire = job.clone();
        let running_for_fire = running.clone();
        let chain_for_fire = chain_parent.clone();
        tokio::spawn(async move {
            run_one_fire(&mut agent_for_fire, &job_for_fire, next, &chain_for_fire).await;
            running_for_fire.store(false, Ordering::SeqCst);
        });
    }
}

/// Recover the chain anchor after a scheduler restart: the tip of this job's
/// successful chain (traces tagged `extra.cron`). The tip is the successful
/// trace no other successful trace of the job depends on, so it follows the
/// `depends_on` lineage rather than `created_at` (a resumed trace inherits the
/// parent's `created_at`, so timestamps tie along a chain and cannot order it).
async fn recover_chain_parent(
    agent: &huggr_agent::Agent,
    job_name: &str,
) -> Option<huggr_agent::TraceId> {
    let heads = match agent.traces.list().await {
        Ok(heads) => heads,
        Err(err) => {
            eprintln!("cron `{job_name}` could not list traces to recover the chain: {err}");
            return None;
        }
    };
    let mut successes = Vec::new();
    let mut parents = std::collections::HashSet::new();
    for head in heads {
        if head.status != huggr_agent::STATUS_SUCCESS || head.agent_name != agent.name {
            continue;
        }
        // `extra` is not part of the head projection, so the job tag requires
        // loading the candidate trace. This runs once per job at startup.
        let Ok(trace) = agent.traces.get(&head.trace_id).await else {
            continue;
        };
        if trace.meta.extra["cron"] != job_name {
            continue;
        }
        if let Some(parent) = &head.depends_on {
            parents.insert(parent.clone());
        }
        successes.push(head.trace_id);
    }
    // The tip is the one success not depended on by another success. Ties are
    // impossible on a linear chain; the id sort only makes an unexpected fork
    // deterministic.
    successes.sort();
    successes.into_iter().find(|id| !parents.contains(id))
}

async fn run_one_fire(
    agent: &mut huggr_agent::Agent,
    job: &CronJobConfig,
    fired_at: DateTime<Utc>,
    chain_parent: &Arc<Mutex<Option<huggr_agent::TraceId>>>,
) {
    let trace_id = if job.lineage == "chain" {
        chain_parent.lock().await.clone()
    } else {
        None
    };
    let ask = huggr_agent::Ask {
        question: job.question.clone(),
        trace_id,
        extra: json!({
            "cron": job.name,
            "fired_at": fired_at.to_rfc3339(),
        }),
        ..huggr_agent::Ask::default()
    };
    match agent.ask(ask).await {
        Ok(answer) => {
            eprintln!(
                "cron `{}` wrote trace {} status={}",
                job.name, answer.trace_id, answer.status
            );
            if job.lineage == "chain" && answer.status == huggr_agent::STATUS_SUCCESS {
                *chain_parent.lock().await = Some(answer.trace_id);
            }
        }
        Err(err) => {
            eprintln!("cron `{}` failed: {err}", job.name);
        }
    }
}

fn effective_limits(base: &LimitsConfig, override_limits: &LimitsConfig) -> LimitsConfig {
    LimitsConfig {
        max_model_calls: override_limits.max_model_calls.or(base.max_model_calls),
        max_cost_micro_usd: override_limits
            .max_cost_micro_usd
            .or(base.max_cost_micro_usd),
        timeout_s: override_limits.timeout_s.or(base.timeout_s),
    }
}

impl From<LimitsConfig> for huggr_agent::AgentLimits {
    fn from(value: LimitsConfig) -> Self {
        let mut limits = huggr_agent::AgentLimits::new();
        if let Some(v) = value.max_model_calls {
            limits = limits.with_max_model_calls(v);
        }
        if let Some(v) = value.max_cost_micro_usd {
            limits = limits.with_max_cost_micro_usd(v);
        }
        if let Some(v) = value.timeout_s {
            limits = limits.with_timeout_ms(v.saturating_mul(1000));
        }
        limits
    }
}

impl From<huggr_agent::AgentLimits> for LimitsConfig {
    fn from(value: huggr_agent::AgentLimits) -> Self {
        LimitsConfig {
            max_model_calls: value.max_model_calls,
            max_cost_micro_usd: value.max_cost_micro_usd,
            timeout_s: value.timeout_ms.map(|ms| ms / 1000),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use async_trait::async_trait;
    use chrono::{TimeZone, Timelike};
    use huggr_agent::{Agent, STATUS_SUCCESS, TraceStore};
    use huggr_core::{ModelOutput, ModelRequest, ModelSelector, Usage};
    use huggr_host::{Clock, ModelAdapter, ModelSink};

    struct MockModel {
        replies: StdMutex<VecDeque<String>>,
    }

    impl MockModel {
        fn new(replies: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                replies: StdMutex::new(replies.into_iter().map(String::from).collect()),
            }
        }
    }

    #[async_trait]
    impl ModelAdapter for MockModel {
        async fn call(
            &self,
            _request: ModelRequest,
            sink: &ModelSink,
        ) -> anyhow::Result<(ModelOutput, Usage)> {
            let text = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock ran out of replies"))?;
            sink.text(text.clone());
            Ok((ModelOutput::text(text), Usage::new(1, 1)))
        }
    }

    fn deterministic_clock() -> Clock {
        let counter = Arc::new(AtomicU64::new(1));
        Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
    }

    fn temp_store(name: &str) -> (TraceStore, std::path::PathBuf) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("huggr-cron-{name}-{}-{n}", std::process::id()));
        (TraceStore::new(&dir), dir)
    }

    fn test_agent(store: TraceStore, replies: impl IntoIterator<Item = &'static str>) -> Agent {
        let mut agent = Agent::new("cron-test", "0.1.0", store);
        agent.models.push((
            ModelSelector::named("medium"),
            Arc::new(MockModel::new(replies)),
        ));
        agent.system_prompt = Some("Answer tersely.".into());
        agent.clock = Some(deterministic_clock());
        agent
    }

    fn chain_job(name: &str) -> CronJobConfig {
        CronJobConfig {
            name: name.to_string(),
            schedule: "0 8 * * *".to_string(),
            question: "daily question".to_string(),
            lineage: "chain".to_string(),
            limits: LimitsConfig::default(),
        }
    }

    #[test]
    fn computes_next_fire_from_cron_expression() {
        let now = Utc.with_ymd_and_hms(2026, 7, 10, 7, 59, 0).unwrap();
        let next = next_fire_after("0 8 * * *", now).unwrap();
        assert_eq!(next.hour(), 8);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn uncapped_cron_jobs_are_rejected_by_default() {
        let mut def = AgentDefinition::parse(
            "[agent]\nname = \"x\"\n[models.medium]\nmodel = \"m\"\n[cron.daily]\nschedule = \"0 8 * * *\"\nquestion = \"q\"\n",
            "huggr.toml",
        )
        .unwrap();
        assert!(validate_cost_caps(&def, false).is_err());
        def.limits.max_cost_micro_usd = Some(100);
        assert!(validate_cost_caps(&def, false).is_ok());
        def.limits.max_cost_micro_usd = None;
        assert!(validate_cost_caps(&def, true).is_ok());
    }

    #[tokio::test]
    async fn one_fire_persists_trace_and_chain_parent() {
        let (store, dir) = temp_store("one-fire");
        let mut agent = test_agent(store.clone(), ["first", "second"]);
        let job = chain_job("daily");
        let chain_parent = Arc::new(Mutex::new(None));
        let fired_at = Utc.with_ymd_and_hms(2026, 7, 10, 8, 0, 0).unwrap();

        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        let first = chain_parent.lock().await.clone().expect("first trace id");
        let first_head = store.head(&first).unwrap();
        assert_eq!(first_head.question, "daily question");
        assert_eq!(first_head.status, STATUS_SUCCESS);
        assert_eq!(first_head.depends_on, None);
        let first_trace = store.get(&first).unwrap();
        assert_eq!(first_trace.meta.extra["cron"], "daily");
        assert_eq!(first_trace.meta.extra["fired_at"], fired_at.to_rfc3339());

        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        let second = chain_parent.lock().await.clone().expect("second trace id");
        let second_head = store.head(&second).unwrap();
        assert_eq!(second_head.depends_on, Some(first));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn chain_parent_recovers_last_successful_trace_across_restart() {
        let (store, dir) = temp_store("recover");
        let mut agent = test_agent(store.clone(), ["first", "second"]);
        let job = chain_job("daily");
        let chain_parent = Arc::new(Mutex::new(None));
        let fired_at = Utc.with_ymd_and_hms(2026, 7, 10, 8, 0, 0).unwrap();

        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        let latest = chain_parent.lock().await.clone().expect("second trace id");

        assert_eq!(
            recover_chain_parent(&agent, "daily").await,
            Some(latest.clone())
        );
        assert_eq!(recover_chain_parent(&agent, "other").await, None);

        // The mock is out of replies: the next fire persists an error trace,
        // which recovery must skip in favor of the last successful one.
        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        assert!(
            store
                .list()
                .unwrap()
                .iter()
                .any(|head| head.status != STATUS_SUCCESS)
        );
        assert_eq!(recover_chain_parent(&agent, "daily").await, Some(latest));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn error_answer_does_not_advance_chain_parent() {
        let (store, dir) = temp_store("error-chain");
        let mut agent = test_agent(store.clone(), ["only"]);
        let job = chain_job("daily");
        let chain_parent = Arc::new(Mutex::new(None));
        let fired_at = Utc.with_ymd_and_hms(2026, 7, 10, 8, 0, 0).unwrap();

        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        let first = chain_parent.lock().await.clone().expect("first trace id");
        assert_eq!(store.head(&first).unwrap().status, STATUS_SUCCESS);

        // The mock is out of replies: the fire yields an error answer, which
        // must not become the next fire's parent.
        run_one_fire(&mut agent, &job, fired_at, &chain_parent).await;
        assert_eq!(chain_parent.lock().await.clone(), Some(first.clone()));
        let error_head = store
            .list()
            .unwrap()
            .into_iter()
            .find(|head| head.status != STATUS_SUCCESS)
            .expect("error trace");
        assert_eq!(error_head.depends_on, Some(first));

        let _ = std::fs::remove_dir_all(dir);
    }
}
