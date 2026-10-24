// SPDX-License-Identifier: GPL-3.0-or-later
//! Plans waiting to be applied, idempotency records, and the JSON shown to clients for a plan.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use kicad_domain::{Operation, OperationPlan, Point};
use serde_json::{json, Value};

pub struct StoredPlan {
    pub session_id: String,
    pub plan: OperationPlan,
    /// Document fingerprint the plan was made against.
    pub fingerprint: String,
    /// Idempotency key of the apply that consumed this plan.
    pub applied_with: Option<String>,
}

#[derive(Default)]
pub struct PlanStore {
    plans: BTreeMap<String, StoredPlan>,
    /// Idempotency key → (plan id, recorded tool result envelope).
    applies: BTreeMap<String, (String, Value)>,
}

static NEXT_PLAN: AtomicU64 = AtomicU64::new(1);

impl PlanStore {
    pub fn insert(&mut self, plan: StoredPlan) -> String {
        let id = format!("plan-{:04}", NEXT_PLAN.fetch_add(1, Ordering::Relaxed));
        self.plans.insert(id.clone(), plan);
        id
    }

    pub fn get(&self, id: &str) -> Option<&StoredPlan> {
        self.plans.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut StoredPlan> {
        self.plans.get_mut(id)
    }

    pub fn previous_apply(&self, key: &str) -> Option<&(String, Value)> {
        self.applies.get(key)
    }

    pub fn record_apply(&mut self, key: String, plan_id: String, envelope: Value) {
        self.applies.insert(key, (plan_id, envelope));
    }

    /// Plans of a closed session can never be applied.
    pub fn forget_session(&mut self, session_id: &str) {
        self.plans.retain(|_, p| p.session_id != session_id);
    }
}

fn mm(p: Point) -> Value {
    json!({ "x": p.x.to_mm(), "y": p.y.to_mm() })
}

/// An operation with coordinates in millimetres.
pub fn operation_view(op: &Operation) -> Value {
    match op {
        Operation::PlaceSymbol {
            component_id,
            library_id,
            reference,
            value,
            footprint,
            position,
            orientation,
        } => json!({
            "op": "place_symbol",
            "component_id": component_id,
            "library_id": library_id.to_string(),
            "reference": reference,
            "value": value,
            "footprint": footprint,
            "position_mm": mm(*position),
            "rotation_deg": orientation.rotation_deg,
        }),
        Operation::AddWire { net, start, end } => json!({
            "op": "add_wire", "net": net, "start_mm": mm(*start), "end_mm": mm(*end),
        }),
        Operation::AddLabel { net, position } => json!({
            "op": "add_label", "net": net, "position_mm": mm(*position),
        }),
    }
}

pub fn plan_view(plan_id: &str, plan: &OperationPlan) -> Value {
    let (symbols, wires, labels) = plan.count();
    let can_apply = !kicad_domain::validation::has_errors(&plan.findings);
    json!({
        "plan_id": plan_id,
        "circuit": plan.circuit,
        "can_apply": can_apply,
        "counts": { "symbols": symbols, "wires": wires, "labels": labels },
        "references": plan.references,
        "findings": plan.findings,
        "operations": plan.operations.iter().map(operation_view).collect::<Vec<_>>(),
        "expected_nets": plan.expected_nets,
    })
}
