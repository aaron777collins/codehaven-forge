//! An action's `action.yml`: its inputs, outputs and how it runs. Read by the runner for an
//! action fetched from GitHub, from a repository here, or from the job's own workspace.

use crate::model::Step;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionDef {
    pub name: String,
    /// In file order.
    pub inputs: Vec<ActionInput>,
    /// A composite action's outputs are expressions over its own steps. A JavaScript action's
    /// come from its `$GITHUB_OUTPUT`, so the value is `None`.
    pub outputs: BTreeMap<String, Option<String>>,
    pub runs: ActionRuns,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionInput {
    pub id: String,
    /// May hold `${{ }}`, evaluated with the job's contexts.
    pub default: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionRuns {
    /// `using: node12 | node16 | node20 | node24`. Every one runs with the job image's `node`.
    Node {
        using: String,
        main: String,
        pre: Option<String>,
        pre_if: Option<String>,
        post: Option<String>,
        post_if: Option<String>,
    },
    Composite {
        steps: Vec<Step>,
    },
    /// `using: docker`. Jobs here have no Docker daemon, so these are refused when they run.
    Docker {
        image: String,
    },
}
