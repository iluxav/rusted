//! The published plan limits, as a constant.
//!
//! These are public — they're the pricing page — so local development can warn
//! about what a run would cost on each tier without asking a server. The
//! database remains the authority for enforcement; this table only informs.

/// A plan's limits as far as local development can observe them.
pub struct Tier {
    pub code: &'static str,
    pub name: &'static str,
    pub exec_ms: u64,
    pub outbound_reqs: u32,
    pub max_script_bytes: usize,
}

pub const TIERS: &[Tier] = &[
    Tier {
        code: "dev",
        name: "Dev",
        exec_ms: 50,
        outbound_reqs: 0,
        max_script_bytes: 262_144,
    },
    Tier {
        code: "pro",
        name: "Pro",
        exec_ms: 500,
        outbound_reqs: 2,
        max_script_bytes: 1_048_576,
    },
    Tier {
        code: "extra",
        name: "Extra",
        exec_ms: 30_000,
        outbound_reqs: 10,
        max_script_bytes: 5_242_880,
    },
];

/// The most permissive published tier — what local development allows by
/// default, since nothing deployable exceeds it.
pub fn most_permissive() -> &'static Tier {
    TIERS.last().expect("at least one tier")
}

pub fn by_code(code: &str) -> Option<&'static Tier> {
    TIERS.iter().find(|t| t.code == code)
}

/// What a single run actually used.
#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub exec_ms: u64,
    pub outbound_reqs: u32,
    pub script_bytes: usize,
}

impl Tier {
    fn fits(&self, usage: &Usage) -> bool {
        usage.exec_ms <= self.exec_ms
            && usage.outbound_reqs <= self.outbound_reqs
            && usage.script_bytes <= self.max_script_bytes
    }

    /// Why this usage wouldn't fit, phrased for a developer reading a terminal.
    fn shortfall(&self, usage: &Usage) -> Vec<String> {
        let mut reasons = Vec::new();
        if usage.exec_ms > self.exec_ms {
            reasons.push(format!("{}ms exec over {}ms", usage.exec_ms, self.exec_ms));
        }
        if usage.outbound_reqs > self.outbound_reqs {
            reasons.push(match self.outbound_reqs {
                0 => format!("{} fetch calls, none allowed", usage.outbound_reqs),
                n => format!("{} fetch calls over {n}", usage.outbound_reqs),
            });
        }
        if usage.script_bytes > self.max_script_bytes {
            reasons.push(format!(
                "{} KB script over {} KB",
                usage.script_bytes / 1024,
                self.max_script_bytes / 1024
            ));
        }
        reasons
    }
}

/// A warning for a run that wouldn't fit somewhere it matters, or None when
/// there's nothing worth saying.
///
/// With `plan` known, the only question is whether it fits *your* plan. Without
/// it, the useful answer is the cheapest tier that would work.
pub fn warning(usage: &Usage, plan: Option<&str>) -> Option<String> {
    if let Some(tier) = plan.and_then(by_code) {
        if tier.fits(usage) {
            return None;
        }
        return Some(format!(
            "over your {} plan: {}",
            tier.name,
            tier.shortfall(usage).join(", ")
        ));
    }
    // No plan known: stay quiet unless even the free tier can't take it.
    let cheapest = TIERS.iter().find(|t| t.fits(usage));
    let dev = TIERS.first()?;
    if dev.fits(usage) {
        return None;
    }
    Some(match cheapest {
        Some(tier) => format!(
            "needs {} — {} on {}",
            tier.name,
            dev.shortfall(usage).join(", "),
            dev.name
        ),
        None => format!(
            "over every plan: {}",
            most_permissive().shortfall(usage).join(", ")
        ),
    })
}

#[cfg(test)]
mod tier_advisories {
    use super::{warning, Usage};

    #[test]
    fn a_run_within_the_free_tier_says_nothing() {
        let usage = Usage {
            exec_ms: 10,
            outbound_reqs: 0,
            script_bytes: 1024,
        };
        assert_eq!(warning(&usage, None), None);
        assert_eq!(warning(&usage, Some("dev")), None);
    }

    #[test]
    fn without_a_plan_it_names_the_cheapest_tier_that_fits() {
        let usage = Usage {
            exec_ms: 300,
            outbound_reqs: 1,
            script_bytes: 1024,
        };
        let advisory = warning(&usage, None).expect("300ms exceeds Dev");
        assert!(advisory.contains("Pro"), "{advisory}");
        assert!(
            advisory.contains("50ms"),
            "should say what Dev allows: {advisory}"
        );
    }

    #[test]
    fn with_a_plan_it_only_speaks_about_that_plan() {
        let usage = Usage {
            exec_ms: 300,
            outbound_reqs: 1,
            script_bytes: 1024,
        };
        // Fits Pro exactly, so Pro subscribers hear nothing.
        assert_eq!(warning(&usage, Some("pro")), None);
        let advisory = warning(&usage, Some("dev")).expect("over Dev");
        assert!(advisory.contains("your Dev plan"), "{advisory}");
    }

    #[test]
    fn fetch_on_the_free_tier_is_called_out_plainly() {
        let usage = Usage {
            exec_ms: 10,
            outbound_reqs: 2,
            script_bytes: 1024,
        };
        let advisory = warning(&usage, Some("dev")).expect("Dev allows no fetch");
        assert!(advisory.contains("none allowed"), "{advisory}");
    }

    #[test]
    fn a_run_beyond_every_tier_says_so() {
        let usage = Usage {
            exec_ms: 60_000,
            outbound_reqs: 99,
            script_bytes: 99_000_000,
        };
        let advisory = warning(&usage, None).expect("beyond Extra");
        assert!(advisory.contains("over every plan"), "{advisory}");
    }
}
