//! Shared logic for task completion checks.
//! Used by both oneshot and recurring tasks.

mod actions;
mod completion;
mod scheduling;

pub use actions::*;
pub use completion::*;
pub use scheduling::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_delta_positive_appends() {
        assert_eq!(apply_delta_to_counts(&[], 3), vec![3]);
        assert_eq!(apply_delta_to_counts(&[1, 2], 3), vec![1, 2, 3]);
        assert_eq!(apply_delta_to_counts(&[4], 0), vec![4]);
    }

    #[test]
    fn test_apply_delta_negative_reduces_last_entry() {
        // last entry count >= remaining → reduce it, keep the entry
        assert_eq!(apply_delta_to_counts(&[2, 5], -3), vec![2, 2]);
        assert_eq!(apply_delta_to_counts(&[5], -1), vec![4]);
    }

    #[test]
    fn test_apply_delta_negative_removes_entries() {
        // entry count < remaining → remove entirely and continue
        assert_eq!(apply_delta_to_counts(&[2, 3], -4), vec![1]);
        assert_eq!(apply_delta_to_counts(&[2, 3], -5), Vec::<i32>::new());
        // partial consume across multiple entries
        assert_eq!(apply_delta_to_counts(&[2, 2, 2], -5), vec![1]);
    }

    #[test]
    fn test_apply_delta_negative_more_than_total() {
        // remaining exceeds total → all entries removed
        assert_eq!(apply_delta_to_counts(&[2, 3], -99), Vec::<i32>::new());
        assert_eq!(apply_delta_to_counts(&[], -3), Vec::<i32>::new());
    }

    #[test]
    fn test_apply_delta_negative_reduce_to_zero_removes() {
        // count exactly equal to remaining → entry becomes 0, must be dropped
        assert_eq!(apply_delta_to_counts(&[2, 3], -3), vec![2]);
        assert_eq!(apply_delta_to_counts(&[3], -3), Vec::<i32>::new());
    }

    #[test]
    fn test_availability_passed() {
        let day = 86_400;
        let st = 1_000_000i64;
        let hour = 3600;
        let row = |interval: Option<jiff::Span>, dur: Option<i64>| crate::db::TaskRow {
            id: 1,
            short_id: Some(1),
            name: "t".to_string(),
            body: String::new(),
            priority: 5,
            start_time: Some(st),
            available_duration_secs: dur,
            interval_secs: interval.map(|s| crate::date::span_to_db(&s)),
            target_count: 0,
            optional: 0,
            end_time: None,
            parent: None,
            completions: None,
            last_time: None,
        };

        // Recurring, old origin (60 days ago): the window is anchored to
        // the current interval, not the chain origin.
        let now = st + 60 * day + 10_000;
        let span1 = jiff::Span::new().days(1);
        assert!(
            availability_passed(&row(Some(span1), Some(hour)), now),
            "window ended at interval_start + 1h, now is 2.7h in"
        );
        assert!(
            !availability_passed(&row(Some(span1), Some(hour)), st + 60 * day + 1800),
            "window still open 30min in"
        );
        // Exactly at the window end → passed (<=).
        assert!(availability_passed(
            &row(Some(jiff::Span::new().days(1)), Some(hour)),
            st + 60 * day + hour
        ));
        // dur >= interval: the window covers the whole interval → never passed.
        assert!(!availability_passed(
            &row(Some(jiff::Span::new().days(1)), Some(2 * day)),
            st + 60 * day + 1800
        ));
        // No duration → never passed.
        assert!(!availability_passed(
            &row(Some(jiff::Span::new().days(1)), None),
            now
        ));
        assert!(!availability_passed(&row(None, None), now));

        // Scheduled: the window is absolute.
        assert!(availability_passed(&row(None, Some(hour)), st + 2 * hour));
        assert!(!availability_passed(
            &row(None, Some(3 * hour)),
            st + 2 * hour
        ));
    }

    #[test]
    fn test_current_interval_start() {
        let day = 86_400;
        let start = 1_000_000;
        let span = jiff::Span::new().days(1);
        // now exactly at start → first interval
        assert_eq!(current_interval_start(start, span, start), start);
        // now mid-first-interval → boundary is start
        assert_eq!(current_interval_start(start, span, start + 100), start);
        // now exactly one interval later → second interval starts at start+day
        assert_eq!(
            current_interval_start(start, span, start + day),
            start + day
        );
        // now mid-second-interval → boundary is start+day
        assert_eq!(
            current_interval_start(start, span, start + day + 50),
            start + day
        );
        // now before task start → boundary clamps to task start
        assert_eq!(current_interval_start(start, span, start - 10), start);
        // now many intervals later
        assert_eq!(
            current_interval_start(start, span, start + 10 * day + 123),
            start + 10 * day
        );
    }

    #[test]
    fn test_is_task_done_simple() {
        // target_count = 0: simple done/not-done
        assert!(!is_task_done(0, None));
        assert!(is_task_done(0, Some(1)));
        assert!(is_task_done(0, Some(5)));
    }

    #[test]
    fn test_is_task_done_with_target() {
        // target_count = 3: needs 3 completions
        assert!(!is_task_done(3, None));
        assert!(!is_task_done(3, Some(0)));
        assert!(!is_task_done(3, Some(1)));
        assert!(!is_task_done(3, Some(2)));
        assert!(is_task_done(3, Some(3)));
        assert!(is_task_done(3, Some(5))); // over-completed
    }

    #[test]
    fn test_completion_percentage_simple() {
        // target_count = 0: no percentage
        assert_eq!(completion_percentage(0, None), None);
        assert_eq!(completion_percentage(0, Some(1)), None);
    }

    #[test]
    fn test_completion_percentage_with_target() {
        assert_eq!(completion_percentage(4, None), Some(0.0));
        assert_eq!(completion_percentage(4, Some(0)), Some(0.0));
        assert_eq!(completion_percentage(4, Some(1)), Some(25.0));
        assert_eq!(completion_percentage(4, Some(2)), Some(50.0));
        assert_eq!(completion_percentage(4, Some(3)), Some(75.0));
        assert_eq!(completion_percentage(4, Some(4)), Some(100.0));
        assert_eq!(completion_percentage(4, Some(5)), Some(125.0)); // over-completed
    }

    #[test]
    fn test_accept_action_scheduled() {
        // Scheduled tasks never prompt: none → done; done → failed;
        // failed → cleared before the window end, done after.
        let now = 1_000_000i64;
        let (start, dur) = (Some(now - 3600), Some(3600)); // window ends at now
        let scheduled = |c| accept_action(c, true, 0, start, dur, now);
        assert_eq!(scheduled(None), AcceptAction::Complete);
        assert_eq!(scheduled(Some(1)), AcceptAction::SetFailed);
        assert_eq!(scheduled(Some(0)), AcceptAction::Clear); // now == window end → before

        let (start, dur) = (Some(now - 7200), Some(3600)); // window ended an hour ago
        let scheduled = |c| accept_action(c, true, 0, start, dur, now);
        assert_eq!(scheduled(None), AcceptAction::Complete);
        assert_eq!(scheduled(Some(1)), AcceptAction::SetFailed);
        assert_eq!(scheduled(Some(0)), AcceptAction::Complete); // after → done
    }

    #[test]
    fn test_accept_action_once_only_and_target_one() {
        // Once-only and target-1 tasks just toggle: not done → Complete,
        // done → Reset (no modal).
        let now = 1_000_000i64;
        assert_eq!(
            accept_action(None, false, 0, None, None, now),
            AcceptAction::Complete
        );
        assert_eq!(
            accept_action(Some(0), false, 0, None, None, now),
            AcceptAction::Complete
        );
        assert_eq!(
            accept_action(Some(1), false, 0, None, None, now),
            AcceptAction::Reset
        );
        assert_eq!(
            accept_action(None, false, 1, None, None, now),
            AcceptAction::Complete
        );
        assert_eq!(
            accept_action(Some(1), false, 1, None, None, now),
            AcceptAction::Reset
        );
        assert_eq!(
            accept_action(Some(0), false, 1, None, None, now),
            AcceptAction::Complete
        );
    }

    #[test]
    fn test_accept_action_multi_target() {
        // target_count > 1: prompt paths — CompleteModal when not complete,
        // ResetConfirm when complete.
        let now = 1_000_000i64;
        assert_eq!(
            accept_action(None, false, 5, None, None, now),
            AcceptAction::CompletePrompt
        );
        assert_eq!(
            accept_action(Some(0), false, 5, None, None, now),
            AcceptAction::CompletePrompt
        );
        assert_eq!(
            accept_action(Some(2), false, 5, None, None, now),
            AcceptAction::CompletePrompt
        );
        assert_eq!(
            accept_action(Some(5), false, 5, None, None, now),
            AcceptAction::ResetConfirm
        );
        assert_eq!(
            accept_action(Some(7), false, 5, None, None, now),
            AcceptAction::ResetConfirm
        );
    }
}
