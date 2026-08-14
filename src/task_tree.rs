//! Task tree: the parent/child task hierarchy loaded from the database.
//!
//! [`TaskTree::load`] fetches only the rows reachable from the root task
//! (a recursive CTE over `todos.parent`, bounded by `UNION` de-duplication)
//! and assembles them into [`TaskTreeNode`]s. Nothing in the CLI writes
//! `parent` yet — the tree is currently read-only, for future views.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::db::TaskRow;

/// A task and its descendants, rooted at one task.
#[derive(Debug, Clone)]
pub struct TaskTree {
    pub root: TaskTreeNode,
}

/// One node of a task tree: the underlying row plus its children.
#[derive(Debug, Clone)]
pub struct TaskTreeNode {
    pub row: TaskRow,
    pub children: Vec<TaskTreeNode>,
}

impl TaskTree {
    /// Load the subtree rooted at `root_id`: the task itself plus every
    /// descendant, in a single query.
    ///
    /// Returns `None` when no task with that id exists. The recursive CTE
    /// uses `UNION` (row de-duplication), so a parent cycle in the data
    /// cannot make the query loop; assembly additionally tracks seen ids so
    /// a corrupt parent link is clipped rather than recursed forever.
    pub async fn load(pool: &SqlitePool, root_id: i64) -> Result<Option<TaskTree>> {
        let now = crate::date::now();
        let rows = sqlx::query_as::<_, TaskRow>(
            r#"WITH RECURSIVE subtree(id) AS (
                   SELECT ? AS id
                   UNION
                   SELECT t.id FROM todos t JOIN subtree s ON t.parent = s.id
               )
               SELECT t.*, NULL AS completions, NULL AS last_time
               FROM todos t
               WHERE t.id IN (SELECT id FROM subtree)
               ORDER BY t.priority DESC, t.start_time ASC, t.id ASC"#,
        )
        .bind(root_id)
        .fetch_all(pool)
        .await
        .context("Failed to fetch task tree")?;

        if rows.is_empty() {
            return Ok(None);
        }

        // Attach interval-scoped completion sums (recurring tasks scope to
        // the current interval via jiff calendar math).
        let rows = crate::db::attach_full_completions(pool, rows, now).await?;

        let mut nodes: HashMap<i64, TaskRow> = HashMap::with_capacity(rows.len());
        let mut children_of: HashMap<i64, Vec<i64>> = HashMap::new();
        // Iterate the rows in query order (priority, start time, id) so the
        // sibling order in each `children` vec is stable and meaningful;
        // HashMap iteration order would randomize it.
        for r in rows {
            if let Some(parent) = r.parent {
                children_of.entry(parent).or_default().push(r.id);
            }
            nodes.insert(r.id, r);
        }

        let mut seen = HashSet::new();
        let root = assemble(root_id, &mut nodes, &children_of, &mut seen)
            .expect("the CTE seed guarantees the root row is present");
        Ok(Some(TaskTree { root }))
    }

    /// Render the subtree below the root (the root row itself is the task
    /// already shown by the caller, e.g. the preview heading) as plain
    /// text lines, depth-first. Each node is rendered by `render_row`,
    /// which returns the full row text — badge, label, and body included
    /// (e.g. `- {badge} {label}\n{body}`); every line of the returned
    /// string is prefixed with the current indent. `indent` is the
    /// initial indent of the first level, and each deeper level adds
    /// `indent_count` more. The caller owns the row shape — glyphs,
    /// label, body layout.
    pub fn draw(
        &self,
        indent: usize,
        indent_count: usize,
        render_row: impl Fn(&TaskRow) -> String,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for child in &self.root.children {
            push_node(child, indent, indent_count, &render_row, &mut out);
        }
        out
    }
}

/// Append `node` and its subtree to `out`, depth-first, each level
/// indented `indent` spaces beyond the parent's.
fn push_node(
    node: &TaskTreeNode,
    indent: usize,
    indent_count: usize,
    render_row: &impl Fn(&TaskRow) -> String,
    out: &mut Vec<String>,
) {
    for line in render_row(&node.row).lines() {
        out.push(format!("{}{}", " ".repeat(indent), line));
    }
    for child in &node.children {
        push_node(child, indent + indent_count, indent_count, render_row, out);
    }
}

/// Build the node subtree rooted at `id`, removing visited rows from
/// `nodes` so each row is emitted exactly once. Returns `None` when the id
/// was already visited on this path (a parent cycle) — the back edge is
/// clipped.
fn assemble(
    id: i64,
    nodes: &mut HashMap<i64, TaskRow>,
    children_of: &HashMap<i64, Vec<i64>>,
    seen: &mut HashSet<i64>,
) -> Option<TaskTreeNode> {
    if !seen.insert(id) {
        return None;
    }
    let mut node = TaskTreeNode {
        row: nodes.remove(&id)?,
        children: Vec::new(),
    };
    if let Some(kids) = children_of.get(&id) {
        for kid in kids {
            if let Some(child) = assemble(*kid, nodes, children_of, seen) {
                node.children.push(child);
            }
        }
    }
    Some(node)
}

#[cfg(test)]
mod tests {
    use crate::db::test_pool;
    use crate::db::{TaskObject, create_task};
    use crate::types::TaskKind;

    use super::*;

    /// Insert a task via the typed API; returns its row id.
    async fn seed_task(
        pool: &sqlx::SqlitePool,
        name: &str,
        parent: Option<i64>,
        target_count: i32,
        interval_secs: Option<i64>,
    ) -> i64 {
        let (id, _short) = create_task(
            pool,
            &TaskObject {
                id: None,
                short_id: None,
                name: name.to_string(),
                body: format!("body of {name}"),
                priority: 5,
                start_time: Some(1_700_000_000),
                available_duration_secs: None,
                interval_secs,
                target_count,
                optional: false,
                end_time: None,
                parent,
            },
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn test_load_builds_subtree() {
        let pool = test_pool().await.unwrap();
        let root = seed_task(&pool, "root", None, 0, None).await;
        let child = seed_task(&pool, "child", Some(root), 2, None).await;
        let grandchild = seed_task(&pool, "grandchild", Some(child), 0, None).await;
        let recurring = seed_task(&pool, "recurring", Some(child), 0, Some(86_400)).await;
        // Unrelated task must not leak into the tree.
        seed_task(&pool, "other", None, 0, None).await;

        let tree = TaskTree::load(&pool, root).await.unwrap().unwrap();
        assert_eq!(tree.root.row.id, root);
        assert_eq!(tree.root.row.name, "root");
        assert_eq!(tree.root.row.kind(), TaskKind::Oneshot);
        // No completions: SUM over zero rows is NULL, so the raw row holds
        // `None` (render treats it as 0).
        assert_eq!(tree.root.row.completions, None);
        assert_eq!(tree.root.children.len(), 1);

        let child_node = &tree.root.children[0];
        assert_eq!(child_node.row.id, child);
        assert_eq!(child_node.row.body, "body of child");
        assert_eq!(child_node.row.target_count, 2);
        assert_eq!(child_node.row.kind(), TaskKind::Oneshot);
        assert_eq!(child_node.children.len(), 2);
        assert_eq!(child_node.children[0].row.id, grandchild);
        assert_eq!(child_node.children[0].row.kind(), TaskKind::Oneshot);
        assert_eq!(child_node.children[1].row.id, recurring);
        assert_eq!(child_node.children[1].row.kind(), TaskKind::Recurring);
    }

    #[tokio::test]
    async fn test_load_missing_root_returns_none() {
        let pool = test_pool().await.unwrap();
        assert!(TaskTree::load(&pool, 42).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_load_clips_parent_cycles() {
        let pool = test_pool().await.unwrap();
        let a = seed_task(&pool, "a", None, 0, None).await;
        let b = seed_task(&pool, "b", Some(a), 0, None).await;
        // Corrupt a's parent to point back at b: a <-> b cycle.
        sqlx::query("UPDATE todos SET parent = ? WHERE id = ?")
            .bind(b)
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();

        let tree = TaskTree::load(&pool, a).await.unwrap().unwrap();
        assert_eq!(tree.root.row.id, a);
        assert_eq!(tree.root.children.len(), 1);
        let b_node = &tree.root.children[0];
        assert_eq!(b_node.row.id, b);
        // The back edge a <- b is clipped: b has no children.
        assert!(b_node.children.is_empty());
    }

    #[tokio::test]
    async fn test_load_scopes_recurring_completions_to_interval() {
        let pool = test_pool().await.unwrap();
        // Recurring task anchored in the past; interval 1 day.
        let (id, _) = create_task(
            &pool,
            &TaskObject {
                id: None,
                short_id: None,
                name: "daily".to_string(),
                body: String::new(),
                priority: 5,
                start_time: Some(1_600_000_000),
                available_duration_secs: None,
                interval_secs: Some(crate::date::span_to_db(&jiff::Span::new().days(1))),
                target_count: 1,
                optional: false,
                end_time: None,
                parent: None,
            },
        )
        .await
        .unwrap();
        // Completion in a *previous* interval (2 days before the start of
        // the current one) must not count.
        sqlx::query("INSERT INTO todo_completions (todo_id, time, count) VALUES (?, ?, ?)")
            .bind(id)
            .bind(1_600_000_000 + 100)
            .bind(1)
            .execute(&pool)
            .await
            .unwrap();

        let tree = TaskTree::load(&pool, id).await.unwrap().unwrap();
        assert_eq!(tree.root.row.completions, None);
    }

    #[tokio::test]
    async fn test_draw_rows_with_optional_body() {
        let pool = test_pool().await.unwrap();
        let root = seed_task(&pool, "root", None, 0, None).await;
        let _child = seed_task(&pool, "child", Some(root), 3, None).await;
        seed_task(&pool, "sibling", Some(root), 0, None).await;

        let tree = TaskTree::load(&pool, root).await.unwrap().unwrap();
        // The subtree below the root: one row per task, depth-first; the
        // closure owns the full row text (badge, label, body).
        let draw = |indent: usize| {
            tree.draw(indent, 2, |row: &TaskRow| {
                if row.body.is_empty() {
                    format!("- {}", row.name)
                } else {
                    format!("- {}\n  {}", row.name, row.body)
                }
            })
        };

        // Indent 0: rows at column 0, bodies at 2 (the caller's own
        // relative indent), depth-first: child, sibling.
        let lines = draw(0);
        assert_eq!(
            lines,
            vec![
                "- child",
                "  body of child",
                "- sibling",
                "  body of sibling",
            ]
        );

        // Indent 2 (the preview's subtree): every line shifted by 2.
        let lines = draw(2);
        assert_eq!(
            lines,
            vec![
                "  - child",
                "    body of child",
                "  - sibling",
                "    body of sibling",
            ]
        );
    }

    #[tokio::test]
    async fn test_draw_applies_render_closure_per_row_and_indents_depth() {
        let pool = test_pool().await.unwrap();
        let root = seed_task(&pool, "root", None, 0, None).await;
        let child = seed_task(&pool, "child", Some(root), 0, None).await;
        seed_task(&pool, "grandchild", Some(child), 0, None).await;

        let tree = TaskTree::load(&pool, root).await.unwrap().unwrap();
        // The closure runs per row with access to the whole row (here: a
        // completion marker for one specific task); deeper levels indent
        // by 2 per depth.
        let lines = tree.draw(2, 2, |row: &TaskRow| {
            if row.name == "child" {
                format!("- ● {}", row.name)
            } else {
                format!("- {}", row.name)
            }
        });
        assert_eq!(lines, vec!["  - ● child", "    - grandchild"]);
    }
}
