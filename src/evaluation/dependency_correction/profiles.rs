use super::CorrectionImpact;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scope {
    profile: String,
    selector: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rows {
    ranges: Vec<[u64; 2]>,
    columns: Option<BTreeSet<String>>,
}

impl Rows {
    fn valid(&self) -> bool {
        !self.ranges.is_empty()
            && self.ranges.iter().all(|[start, end]| start < end)
            && self.ranges.windows(2).all(|pair| pair[0][1] <= pair[1][0])
            && self.columns.as_ref().is_none_or(|columns| {
                !columns.is_empty() && columns.iter().all(|name| !name.is_empty())
            })
    }

    fn overlaps(&self, other: &Self) -> bool {
        if let (Some(notice), Some(usage)) = (&self.columns, &other.columns)
            && notice.is_disjoint(usage)
        {
            return false;
        }
        let mut other = other.ranges.iter().peekable();
        for &[start, end] in &self.ranges {
            while other.next_if(|range| range[1] <= start).is_some() {}
            if other.peek().is_some_and(|range| range[0] < end) {
                return true;
            }
        }
        false
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstrumentTime {
    time_basis: String,
    start: i64,
    end: i64,
    mode: String,
}

impl InstrumentTime {
    fn valid(&self) -> bool {
        self.start < self.end && !self.time_basis.is_empty() && !self.mode.is_empty()
    }
}

impl Scope {
    pub(super) fn overlap(&self, use_scope: &Scope) -> CorrectionImpact {
        use CorrectionImpact::{Affected, Open, Outside};
        if self.profile != use_scope.profile {
            return Open;
        }
        let overlaps = match self.profile.as_str() {
            "table-rows-1" => {
                let (Ok(notice), Ok(usage)) = (
                    Rows::deserialize(&self.selector),
                    Rows::deserialize(&use_scope.selector),
                ) else {
                    return Open;
                };
                if !notice.valid() || !usage.valid() {
                    return Open;
                }
                notice.overlaps(&usage)
            }
            "instrument-time-1" => {
                let (Ok(notice), Ok(usage)) = (
                    InstrumentTime::deserialize(&self.selector),
                    InstrumentTime::deserialize(&use_scope.selector),
                ) else {
                    return Open;
                };
                if !notice.valid() || !usage.valid() || notice.time_basis != usage.time_basis {
                    return Open;
                }
                notice.start < usage.end && usage.start < notice.end && notice.mode == usage.mode
            }
            _ => return Open,
        };
        if overlaps { Affected } else { Outside }
    }
}
