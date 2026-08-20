//! Решение supervisor'а о перезапуске сервиса.

use rustos_abi::ExitReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartPolicy {
    Never,
    /// Перезапускать только fault/отрицательный status, с ограничением числа
    /// попыток и экспоненциальной задержкой.
    OnFault {
        max_restarts: u8,
        base_backoff_ticks: u64,
        max_backoff_ticks: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartDecision {
    CleanStop,
    PolicyDisabled,
    RestartAt(u64),
    Exhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorState {
    policy: RestartPolicy,
    consecutive_failures: u8,
}

impl SupervisorState {
    pub const fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            consecutive_failures: 0,
        }
    }

    pub const fn consecutive_failures(&self) -> u8 {
        self.consecutive_failures
    }

    /// Стабильно работающий сервис сбрасывает накопленную backoff-серию.
    pub fn mark_healthy(&mut self) {
        self.consecutive_failures = 0;
    }

    pub fn record_exit(&mut self, reason: ExitReason, now: u64) -> RestartDecision {
        if reason.exception == 0 && reason.status >= 0 {
            self.consecutive_failures = 0;
            return RestartDecision::CleanStop;
        }
        let RestartPolicy::OnFault {
            max_restarts,
            base_backoff_ticks,
            max_backoff_ticks,
        } = self.policy
        else {
            return RestartDecision::PolicyDisabled;
        };
        if self.consecutive_failures >= max_restarts {
            return RestartDecision::Exhausted;
        }
        let multiplier = 1u64
            .checked_shl(self.consecutive_failures as u32)
            .unwrap_or(u64::MAX);
        let delay = base_backoff_ticks
            .saturating_mul(multiplier)
            .min(max_backoff_ticks);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        RestartDecision::RestartAt(now.saturating_add(delay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAULT: ExitReason = ExitReason {
        status: -1,
        exception: 13,
        flags: 0,
        fault_address: 0,
    };

    #[test]
    fn restart_is_bounded_and_backed_off() {
        let mut state = SupervisorState::new(RestartPolicy::OnFault {
            max_restarts: 3,
            base_backoff_ticks: 10,
            max_backoff_ticks: 100,
        });
        assert_eq!(
            state.record_exit(FAULT, 100),
            RestartDecision::RestartAt(110)
        );
        assert_eq!(
            state.record_exit(FAULT, 100),
            RestartDecision::RestartAt(120)
        );
        assert_eq!(
            state.record_exit(FAULT, 100),
            RestartDecision::RestartAt(140)
        );
        assert_eq!(state.record_exit(FAULT, 100), RestartDecision::Exhausted);
    }
}
