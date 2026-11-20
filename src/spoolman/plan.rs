//! Who gets how much: turns one finished job into the amounts to take off each loaded spool.
//!
//! Klipper measures one number per job, `filament_used` (mm pushed through the extruder(s)),
//! which counts cancelled and failed prints only up to where they stopped. That number is exact
//! for a printer with one spool, and printpit books it as a length so Spoolman converts it with
//! the spool's own density and diameter.
//!
//! With several tools (extruders or multi-material lanes) Klipper's total doesn't say which tool
//! used what, so printpit splits it by the slicer's per-filament weights from the file's
//! metadata (`; filament used [g] = a, b, …`, index = tool), scaled by how much of the planned
//! filament was actually used. A job that used only one tool (by the weights, or by
//! `referenced_tools`) is booked to that tool as an exact length.

use serde::{Deserialize, Serialize};

use super::client::Amount;
use crate::moonraker::types::FileMetadata;

/// One amount for one spool. Stored as JSON in `spoolman_bookings.parts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Part {
    pub tool: i64,
    pub spool_id: i64,
    /// Millimetres, or …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<f64>,
    /// … grams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
    #[serde(default)]
    pub done: bool,
}

impl Part {
    fn new(tool: usize, spool_id: i64, amount: Amount) -> Self {
        let (length, weight) = match amount {
            Amount::Length(mm) => (Some(round2(mm)), None),
            Amount::Grams(g) => (None, Some(round2(g))),
        };
        Self {
            tool: tool as i64,
            spool_id,
            length,
            weight,
            done: false,
        }
    }

    pub fn amount(&self) -> Amount {
        match (self.length, self.weight) {
            (Some(mm), _) => Amount::Length(mm),
            (None, Some(g)) => Amount::Grams(g),
            (None, None) => Amount::Length(0.0),
        }
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub parts: Vec<Part>,
    /// Why something wasn't booked, or how a split was made. Shown with the booking.
    pub notes: Vec<String>,
}

/// `slots[i]` is the spool loaded in tool `i`, if any. `used_mm` is the job's `filament_used`.
pub fn plan(used_mm: f64, meta: &FileMetadata, slots: &[Option<i64>]) -> Plan {
    let mut out = Plan::default();
    if !used_mm.is_finite() || used_mm <= 0.0 {
        out.notes.push("No filament used.".into());
        return out;
    }
    let tool_name = |t: usize| {
        if slots.len() > 1 {
            format!("T{t}")
        } else {
            "the printer".into()
        }
    };
    let book = |out: &mut Plan, tool: usize, amount: Amount| match slots.get(tool).copied().flatten() {
        Some(spool) => out.parts.push(Part::new(tool, spool, amount)),
        None => out.notes.push(format!(
            "{} has no spool assigned; {} not booked.",
            tool_name(tool),
            amount.describe()
        )),
    };
    if slots.len() <= 1 {
        book(&mut out, 0, Amount::Length(used_mm));
        return out;
    }

    let weights: Vec<f64> = meta
        .filament_weights
        .iter()
        .flatten()
        .map(|w| if w.is_finite() && *w > 0.0 { *w } else { 0.0 })
        .collect();
    let mut used_tools: Vec<usize> = weights
        .iter()
        .enumerate()
        .filter(|(_, w)| **w > 0.0)
        .map(|(t, _)| t)
        .collect();
    if used_tools.is_empty() {
        used_tools = meta
            .referenced_tools
            .iter()
            .flatten()
            .filter_map(|t| usize::try_from(*t).ok())
            .collect();
        used_tools.sort_unstable();
        used_tools.dedup();
    }

    match used_tools.as_slice() {
        [tool] => book(&mut out, *tool, Amount::Length(used_mm)),
        [] => {
            // The G-code doesn't say. With exactly one spool loaded, that's the one it used.
            let loaded: Vec<usize> = (0..slots.len()).filter(|t| slots[*t].is_some()).collect();
            if let [tool] = loaded.as_slice() {
                book(&mut out, *tool, Amount::Length(used_mm));
                out.notes.push(format!(
                    "The file doesn't say which tools it used; booked to T{tool}, the only one with a spool."
                ));
            } else {
                out.notes.push(format!(
                    "The file doesn't say which tools it used, so the {:.2} m couldn't be split.",
                    used_mm / 1000.0
                ));
            }
        }
        _ => {
            let planned_mm = meta.filament_total.filter(|t| t.is_finite() && *t > 0.0);
            match (weights.iter().sum::<f64>(), planned_mm) {
                (total_g, Some(planned)) if total_g > 0.0 => {
                    let scale = used_mm / planned;
                    for tool in used_tools {
                        book(&mut out, tool, Amount::Grams(weights[tool] * scale));
                    }
                    if (scale - 1.0).abs() > 0.02 {
                        out.notes.push(format!(
                            "Split by the slicer's per-tool weights, scaled to the {:.0}% of the planned filament that was used.",
                            scale * 100.0
                        ));
                    } else {
                        out.notes.push("Split by the slicer's per-tool weights.".into());
                    }
                }
                _ => out.notes.push(format!(
                    "The file doesn't say how much each tool used, so the {:.2} m couldn't be split.",
                    used_mm / 1000.0
                )),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(weights: Option<Vec<f64>>, tools: Option<Vec<i64>>, total: Option<f64>) -> FileMetadata {
        FileMetadata {
            filament_weights: weights,
            referenced_tools: tools,
            filament_total: total,
            ..FileMetadata::default()
        }
    }

    fn summary(p: &Plan) -> Vec<(i64, i64, Amount)> {
        p.parts.iter().map(|x| (x.tool, x.spool_id, x.amount())).collect()
    }

    #[test]
    fn one_spool_gets_the_measured_length() {
        let p = plan(
            1520.456,
            &meta(Some(vec![4.0, 2.0]), None, Some(3000.0)),
            &[Some(7)],
        );
        assert_eq!(summary(&p), [(0, 7, Amount::Length(1520.46))]);
        assert!(p.notes.is_empty());
    }

    #[test]
    fn nothing_used_or_nothing_loaded() {
        let p = plan(0.0, &FileMetadata::default(), &[Some(7)]);
        assert!(p.parts.is_empty());
        assert_eq!(p.notes, ["No filament used."]);
        assert!(
            plan(f64::NAN, &FileMetadata::default(), &[Some(7)])
                .parts
                .is_empty()
        );
        let p = plan(1000.0, &FileMetadata::default(), &[None]);
        assert!(p.parts.is_empty());
        assert_eq!(p.notes, ["the printer has no spool assigned; 1.00 m not booked."]);
    }

    #[test]
    fn splits_by_weight_scaled_to_what_was_used() {
        let m = meta(Some(vec![6.0, 0.0, 2.0]), None, Some(4000.0));
        // Completed: the whole plan.
        let p = plan(4000.0, &m, &[Some(1), Some(2), Some(3)]);
        assert_eq!(
            summary(&p),
            [(0, 1, Amount::Grams(6.0)), (2, 3, Amount::Grams(2.0))]
        );
        assert_eq!(p.notes, ["Split by the slicer's per-tool weights."]);
        // Cancelled halfway.
        let p = plan(2000.0, &m, &[Some(1), Some(2), Some(3)]);
        assert_eq!(
            summary(&p),
            [(0, 1, Amount::Grams(3.0)), (2, 3, Amount::Grams(1.0))]
        );
        assert!(p.notes[0].contains("50%"), "{:?}", p.notes);
    }

    #[test]
    fn a_single_tool_job_on_a_multi_tool_printer_is_exact() {
        let slots = [Some(1), Some(2)];
        let p = plan(900.0, &meta(Some(vec![0.0, 3.0]), None, Some(1000.0)), &slots);
        assert_eq!(summary(&p), [(1, 2, Amount::Length(900.0))]);
        let p = plan(900.0, &meta(None, Some(vec![1, 1]), None), &slots);
        assert_eq!(summary(&p), [(1, 2, Amount::Length(900.0))]);
    }

    #[test]
    fn unassigned_tools_are_noted_and_the_rest_booked() {
        let m = meta(Some(vec![1.0, 1.0]), None, Some(1000.0));
        let p = plan(1000.0, &m, &[Some(1), None]);
        assert_eq!(summary(&p), [(0, 1, Amount::Grams(1.0))]);
        assert!(
            p.notes
                .iter()
                .any(|n| n == "T1 has no spool assigned; 1.0 g not booked."),
            "{:?}",
            p.notes
        );
    }

    #[test]
    fn unknown_split() {
        // No per-tool data, two spools loaded: can't tell.
        let p = plan(1000.0, &FileMetadata::default(), &[Some(1), Some(2)]);
        assert!(p.parts.is_empty());
        assert!(p.notes[0].contains("couldn't be split"));
        // ... but with one spool loaded, that's it.
        let p = plan(1000.0, &FileMetadata::default(), &[None, Some(2)]);
        assert_eq!(summary(&p), [(1, 2, Amount::Length(1000.0))]);
        // Several tools referenced, no weights.
        let p = plan(1000.0, &meta(None, Some(vec![0, 1]), None), &[Some(1), Some(2)]);
        assert!(p.parts.is_empty());
        assert!(p.notes[0].contains("how much each tool used"));
        // Weights but no planned total to scale by.
        let p = plan(
            1000.0,
            &meta(Some(vec![1.0, 1.0]), None, None),
            &[Some(1), Some(2)],
        );
        assert!(p.parts.is_empty());
    }

    #[test]
    fn parts_round_trip_as_json() {
        let p = Part::new(1, 9, Amount::Grams(1.234));
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, r#"{"tool":1,"spool_id":9,"weight":1.23,"done":false}"#);
        let back: Part = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
