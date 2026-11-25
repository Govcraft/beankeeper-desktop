use std::{collections::HashMap, str::FromStr, time::Instant};

use chrono::Utc;
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use rmcp::model::{CreateTaskResult, DetailedTask, GetTaskResult, Task, TaskPayload, TaskStatus};
use serde_json::{Map, Value};

use super::{ImportError, Principal, RETENTION};

pub(super) const TASKS: &str = "io.modelcontextprotocol/tasks";

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct ImportTaskId(MagicTypeId);

impl FromStr for ImportTaskId {
    type Err = ImportError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = value
            .parse::<MagicTypeId>()
            .map_err(|_| ImportError::Unknown)?;
        if id.prefix().as_str() != "statement_task" {
            return Err(ImportError::Unknown);
        }
        Ok(Self(id))
    }
}

enum Phase {
    Queued,
    Importing,
    Finished(Outcome),
}

enum Outcome {
    Completed(Map<String, Value>),
    Failed(Map<String, Value>),
    Cancelled,
}

impl From<&Outcome> for TaskPayload {
    fn from(outcome: &Outcome) -> Self {
        match outcome {
            Outcome::Completed(result) => Self::Completed {
                result: result.clone(),
            },
            Outcome::Failed(error) => Self::Failed {
                error: error.clone(),
            },
            Outcome::Cancelled => Self::Cancelled,
        }
    }
}

impl Phase {
    fn message(&self) -> &'static str {
        match self {
            Self::Queued => "Waiting to import the statement.",
            Self::Importing => "Checking statement rows and recording the import.",
            Self::Finished(Outcome::Cancelled) => "The import was cancelled before it began.",
            Self::Finished(Outcome::Failed(_)) => "The import could not return a result.",
            Self::Finished(_) => "The import returned its result.",
        }
    }
}

struct ImportTask {
    principal: Principal,
    phase: Phase,
    task: Task,
    created: Instant,
    expires: Instant,
}

impl ImportTask {
    fn set_phase(&mut self, phase: Phase) {
        self.task.last_updated_at = Utc::now().to_rfc3339();
        self.task.status_message = Some(phase.message().into());
        if matches!(phase, Phase::Finished(_)) {
            self.expires = Instant::now() + RETENTION;
            self.task.ttl_ms = u64::try_from((self.expires - self.created).as_millis()).ok();
        }
        self.phase = phase;
    }
}

#[derive(Default)]
pub(super) struct Tasks(HashMap<ImportTaskId, ImportTask>);

impl Tasks {
    const CAPACITY: usize = 16;

    fn prune(&mut self, now: Instant) {
        self.0
            .retain(|_, task| !matches!(task.phase, Phase::Finished(_)) || now < task.expires);
    }

    pub(super) fn enqueue(
        &mut self,
        principal: Principal,
    ) -> Result<(ImportTaskId, CreateTaskResult), ImportError> {
        let now = Instant::now();
        self.prune(now);
        if self.0.len() >= Self::CAPACITY
            || self
                .0
                .values()
                .filter(|task| !matches!(task.phase, Phase::Finished(_)))
                .count()
                >= 2
        {
            return Err(ImportError::Busy);
        }
        let id = ImportTaskId("statement_task".create_type_id::<V7>());
        let timestamp = Utc::now().to_rfc3339();
        let task = Task::new(
            id.0.to_string(),
            TaskStatus::Working,
            &timestamp,
            &timestamp,
        )
        .with_status_message(Phase::Queued.message())
        .with_poll_interval_ms(500);
        self.0.insert(
            id.clone(),
            ImportTask {
                principal,
                task: task.clone(),
                phase: Phase::Queued,
                created: now,
                expires: now + RETENTION,
            },
        );
        Ok((id, CreateTaskResult::new(task)))
    }

    pub(super) fn begin(&mut self, id: &ImportTaskId) -> bool {
        let Some(task) = self
            .0
            .get_mut(id)
            .filter(|task| matches!(task.phase, Phase::Queued))
        else {
            return false;
        };
        task.set_phase(Phase::Importing);
        true
    }

    pub(super) fn finish(
        &mut self,
        id: &ImportTaskId,
        result: Result<Map<String, Value>, Map<String, Value>>,
    ) {
        if let Some(task) = self.0.get_mut(id) {
            let payload = match result {
                Ok(result) => Outcome::Completed(result),
                Err(error) => Outcome::Failed(error),
            };
            task.set_phase(Phase::Finished(payload));
        }
    }

    fn find(
        &mut self,
        id: &ImportTaskId,
        principal: &Principal,
    ) -> Result<&mut ImportTask, ImportError> {
        self.prune(Instant::now());
        self.0
            .get_mut(id)
            .filter(|task| &task.principal == principal)
            .ok_or(ImportError::Unknown)
    }

    pub(super) fn get(
        &mut self,
        id: &ImportTaskId,
        principal: &Principal,
    ) -> Result<GetTaskResult, ImportError> {
        let task = self.find(id, principal)?;
        let payload = match &task.phase {
            Phase::Finished(outcome) => outcome.into(),
            Phase::Queued | Phase::Importing => TaskPayload::Working,
        };
        let detail = DetailedTask::new(task.task.clone(), payload);
        Ok(GetTaskResult::new(detail))
    }

    pub(super) fn cancel(
        &mut self,
        id: &ImportTaskId,
        principal: &Principal,
    ) -> Result<(), ImportError> {
        let task = self.find(id, principal)?;
        if matches!(task.phase, Phase::Queued) {
            task.set_phase(Phase::Finished(Outcome::Cancelled));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::AgentKey;

    #[test]
    fn cancellation_prevents_queued_work_and_preserves_started_import_outcomes()
    -> Result<(), &'static str> {
        let principal = AgentKey::for_test("importer");
        let mut tasks = Tasks::default();
        let enqueue = |tasks: &mut Tasks| {
            tasks
                .enqueue(principal.clone())
                .map(|(id, _)| id)
                .map_err(|_| "could not queue import")
        };
        let queued = enqueue(&mut tasks)?;
        tasks
            .cancel(&queued, &principal)
            .map_err(|_| "could not cancel queued import")?;
        assert!(!tasks.begin(&queued));
        assert_eq!(
            tasks
                .get(&queued, &principal)
                .map_err(|_| "missing queued task")?
                .task
                .status(),
            TaskStatus::Cancelled
        );

        let active = enqueue(&mut tasks)?;
        assert!(tasks.begin(&active));
        tasks
            .cancel(&active, &principal)
            .map_err(|_| "could not request cancellation")?;
        tasks.finish(&active, Ok(Map::new()));
        assert_eq!(
            tasks
                .get(&active, &principal)
                .map_err(|_| "missing completed task")?
                .task
                .status(),
            TaskStatus::Completed
        );
        Ok(())
    }
}
