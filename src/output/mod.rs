mod entry;
mod tasks;
mod today;

pub(crate) fn print_rows(rows: &[(String, String)]) {
    for (label, value) in rows {
        println!("{label:<13}:\t{value}");
    }
}

pub use entry::display_entry;
pub(crate) use tasks::task_rows;
pub use tasks::{format_tasks_simple, task_intro};
pub use today::format_today_simple;
