use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notice {
    message: String,
    severity: NoticeSeverity,
    lifetime: NoticeLifetime,
}

#[derive(Debug, Clone, Copy)]
enum NoticeLifetime {
    Transient {
        started_at: Instant,
        expires_at: Instant,
    },
    Persistent,
}

impl Notice {
    pub fn transient(
        message: impl Into<String>,
        severity: NoticeSeverity,
        now: Instant,
        duration: Duration,
    ) -> Self {
        assert!(!duration.is_zero(), "transient notices need a lifetime");
        Self {
            message: message.into(),
            severity,
            lifetime: NoticeLifetime::Transient {
                started_at: now,
                expires_at: now + duration,
            },
        }
    }

    pub fn persistent(message: impl Into<String>, severity: NoticeSeverity) -> Self {
        Self {
            message: message.into(),
            severity,
            lifetime: NoticeLifetime::Persistent,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn severity(&self) -> NoticeSeverity {
        self.severity
    }

    fn is_expired(&self, now: Instant) -> bool {
        matches!(
            self.lifetime,
            NoticeLifetime::Transient { expires_at, .. } if now >= expires_at
        )
    }

    const fn is_transient(&self) -> bool {
        matches!(self.lifetime, NoticeLifetime::Transient { .. })
    }

    pub fn remaining_fraction(&self, now: Instant) -> Option<f32> {
        let NoticeLifetime::Transient {
            started_at,
            expires_at,
        } = self.lifetime
        else {
            return None;
        };
        let total = expires_at.duration_since(started_at);
        let remaining = expires_at.saturating_duration_since(now);
        Some((remaining.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0))
    }
}

#[derive(Default)]
pub struct NoticeBanner {
    current: Option<Notice>,
}

impl NoticeBanner {
    pub const fn new(current: Option<Notice>) -> Self {
        Self { current }
    }

    pub fn show(&mut self, notice: Notice) {
        self.current = Some(notice);
    }

    pub fn clear(&mut self) {
        self.current = None;
    }

    pub fn expire(&mut self, now: Instant) {
        if self
            .current
            .as_ref()
            .is_some_and(|notice| notice.is_expired(now))
        {
            self.clear();
        }
    }

    pub const fn current(&self) -> Option<&Notice> {
        self.current.as_ref()
    }

    pub fn is_animating(&self) -> bool {
        self.current.as_ref().is_some_and(Notice::is_transient)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_progress_counts_down_to_zero() {
        let started_at = Instant::now();
        let notice = Notice::transient(
            "Unavailable",
            NoticeSeverity::Warning,
            started_at,
            Duration::from_secs(4),
        );

        assert!((notice.remaining_fraction(started_at).unwrap() - 1.0).abs() < f32::EPSILON);
        assert!(
            (notice
                .remaining_fraction(started_at + Duration::from_secs(2))
                .unwrap()
                - 0.5)
                .abs()
                < f32::EPSILON
        );
        assert!(
            notice
                .remaining_fraction(started_at + Duration::from_secs(4))
                .unwrap()
                .abs()
                < f32::EPSILON
        );
        assert!(!notice.is_expired(started_at + Duration::from_millis(3_999)));
        assert!(notice.is_expired(started_at + Duration::from_secs(4)));
    }

    #[test]
    fn persistent_notice_has_no_progress_or_deadline() {
        let notice = Notice::persistent("Database unavailable", NoticeSeverity::Error);
        assert_eq!(notice.severity(), NoticeSeverity::Error);
        assert!(notice.remaining_fraction(Instant::now()).is_none());
        assert!(!notice.is_expired(Instant::now() + Duration::from_secs(86_400)));
    }

    #[test]
    fn banner_replaces_old_messages_and_expires_only_transient_ones() {
        let now = Instant::now();
        let mut banner = NoticeBanner::default();
        banner.show(Notice::persistent("Old fault", NoticeSeverity::Error));
        banner.show(Notice::transient(
            "Latest feedback",
            NoticeSeverity::Info,
            now,
            Duration::from_secs(2),
        ));

        assert_eq!(
            banner.current().map(Notice::message),
            Some("Latest feedback")
        );
        assert!(banner.is_animating());
        banner.expire(now + Duration::from_secs(2));
        assert!(banner.current().is_none());
        assert!(!banner.is_animating());

        banner.show(Notice::persistent("Current fault", NoticeSeverity::Error));
        assert!(!banner.is_animating());
        banner.expire(now + Duration::from_secs(86_400));
        assert_eq!(banner.current().map(Notice::message), Some("Current fault"));
    }
}
