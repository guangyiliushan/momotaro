//! Reading and writing `graph_links`, the derived edge table (ADR 0027).
//!
//! The table is an idempotent projection: one row per as-written identity,
//! replaced wholesale per `from_source_key`. Rows carry no build history —
//! `created_at` is the `ingested_at` of the revision whose content carries the
//! link, so `rm graph_links` + a rebuild reproduces every row byte for byte.

use momotaro_contracts::{AnchorKind, EdgeKind, GraphLink, LinkSource};
use rusqlite::params;

use crate::StoreError;

const GRAPH_COLUMNS: &str = "from_source_key, to_source_key, raw_target, edge_kind, link_source, anchor_kind, anchor_raw, created_at";

fn read_graph_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<GraphLink, StoreError>> {
    let edge_kind: String = row.get("edge_kind")?;
    let link_source: String = row.get("link_source")?;
    let anchor_kind: Option<String> = row.get("anchor_kind")?;
    let parse = || {
        let edge_kind =
            EdgeKind::from_code(&edge_kind).ok_or_else(|| StoreError::UnknownGraphSpelling {
                column: "edge_kind",
                value: edge_kind.clone(),
            })?;
        let link_source = LinkSource::from_code(&link_source).ok_or_else(|| {
            StoreError::UnknownGraphSpelling {
                column: "link_source",
                value: link_source.clone(),
            }
        })?;
        let anchor_kind = anchor_kind
            .as_deref()
            .map(|code| {
                AnchorKind::from_code(code).ok_or_else(|| StoreError::UnknownGraphSpelling {
                    column: "anchor_kind",
                    value: code.to_owned(),
                })
            })
            .transpose()?;
        Ok(GraphLink {
            from_source_key: row.get("from_source_key")?,
            to_source_key: row.get("to_source_key")?,
            raw_target: row.get("raw_target")?,
            edge_kind,
            link_source,
            anchor_kind,
            anchor_raw: row.get("anchor_raw")?,
            created_at: row.get("created_at")?,
        })
    };
    Ok(parse())
}

/// Deletes every row of one source and inserts `edges` in its place.
///
/// Shared by both write paths (the file's content transaction and the
/// projection-only replace), so the identity set cannot be built two ways.
/// The identity set is enforced here — an as-written tuple that appears twice
/// goes in once (ADR 0027) — and every row is written under the
/// `from_source_key` argument, which is also the delete key; a row struct's
/// own `from_source_key` field cannot disagree with it.
pub(crate) fn write_edges_tx(
    tx: &rusqlite::Transaction<'_>,
    from_source_key: &str,
    edges: &[GraphLink],
) -> Result<(), StoreError> {
    tx.execute(
        "DELETE FROM graph_links WHERE from_source_key = ?1",
        params![from_source_key],
    )?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO graph_links (from_source_key, to_source_key, raw_target, edge_kind, \
             link_source, anchor_kind, anchor_raw, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut written: Vec<&GraphLink> = Vec::new();
        for edge in edges {
            let same_identity = written.iter().any(|seen| {
                seen.raw_target == edge.raw_target
                    && seen.edge_kind == edge.edge_kind
                    && seen.link_source == edge.link_source
                    && seen.anchor_kind == edge.anchor_kind
                    && seen.anchor_raw == edge.anchor_raw
            });
            if same_identity {
                continue;
            }
            written.push(edge);
            insert.execute(params![
                from_source_key,
                edge.to_source_key,
                edge.raw_target,
                edge.edge_kind.as_str(),
                edge.link_source.as_str(),
                edge.anchor_kind.map(AnchorKind::as_str),
                edge.anchor_raw,
                edge.created_at,
            ])?;
        }
    }
    Ok(())
}

impl crate::Store {
    /// Replaces every projected source's edges in one transaction (ADR 0027).
    ///
    /// Each pair is still wholesale per `from_source_key`; the batch is the
    /// write unit so a 10k-file run pays one commit for its re-projections
    /// instead of ten thousand. The rows a parse wrote ride the file's own
    /// content transaction and never come through here.
    pub fn replace_edges_many(
        &mut self,
        projections: &[(String, Vec<GraphLink>)],
    ) -> Result<(), StoreError> {
        let tx = self.connection.transaction()?;
        for (from_source_key, edges) in projections {
            write_edges_tx(&tx, from_source_key, edges)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the rows of every source the walk no longer names.
    ///
    /// The other half of "a rebuilt projection reproduces the table" (ADR
    /// 0027): a source that left the vault must not keep rows a rebuild would
    /// never produce. `source_keys_json` is a JSON array of the keys to keep.
    pub fn prune_graph_edges_outside(&mut self, source_keys_json: &str) -> Result<u64, StoreError> {
        let removed = self.connection.execute(
            "DELETE FROM graph_links WHERE from_source_key NOT IN
             (SELECT value FROM json_each(?1))",
            params![source_keys_json],
        )?;
        Ok(u64::try_from(removed).unwrap_or(0))
    }

    /// Row totals for the batch summary: `(rows, of which dangling)`.
    pub fn graph_counts(&self) -> Result<(u64, u64), StoreError> {
        let rows: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM graph_links", [], |row| row.get(0))?;
        let dangling: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM graph_links WHERE to_source_key IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok((
            u64::try_from(rows).unwrap_or(0),
            u64::try_from(dangling).unwrap_or(0),
        ))
    }

    /// Every row, in insertion order — the reader the rebuild-identity check
    /// and `rm graph_links` acceptance compare through.
    pub fn all_graph_links(&self) -> Result<Vec<GraphLink>, StoreError> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {GRAPH_COLUMNS} FROM graph_links ORDER BY rowid"
        ))?;
        let rows = statement.query_map([], read_graph_row)?;
        let mut links = Vec::new();
        for row in rows {
            links.push(row??);
        }
        Ok(links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_store;

    fn edge(target: Option<&str>, raw: &str) -> GraphLink {
        GraphLink {
            from_source_key: "note:a.md".to_owned(),
            to_source_key: target.map(ToOwned::to_owned),
            raw_target: raw.to_owned(),
            edge_kind: EdgeKind::Link,
            link_source: LinkSource::Wikilink,
            anchor_kind: Some(AnchorKind::Heading),
            anchor_raw: Some("H".to_owned()),
            created_at: 7,
        }
    }

    #[test]
    fn replace_edges_is_wholesale_and_idempotent() {
        let (_dir, mut store) = temp_store();
        let rows = vec![
            edge(Some("note:b.md"), "b"),
            edge(None, "gone"),
            edge(None, "gone"), // the same writing twice: one row
        ];
        store
            .replace_edges_many(&[("note:a.md".to_owned(), rows.clone())])
            .expect("write");
        let first = store.all_graph_links().expect("read");
        assert_eq!(first.len(), 2, "identity dedupe at the writer");

        store
            .replace_edges_many(&[("note:a.md".to_owned(), rows.clone())])
            .expect("rewrite");
        let second = store.all_graph_links().expect("read");
        assert_eq!(first, second, "re-running the projection is a no-op");

        // The anchor is half the identity (ADR 0027): the same writing with two
        // anchors is two rows, and a dedupe that ignored the anchor columns
        // would silently merge them.
        let two_anchors = vec![
            edge(Some("note:t.md"), "t"),
            GraphLink {
                anchor_kind: Some(AnchorKind::Block),
                anchor_raw: Some("^id".to_owned()),
                ..edge(Some("note:t.md"), "t")
            },
        ];
        store
            .replace_edges_many(&[("note:a.md".to_owned(), two_anchors.clone())])
            .expect("write anchors");
        assert_eq!(
            store.all_graph_links().expect("read").len(),
            2,
            "the anchor axis is part of the identity"
        );
    }

    #[test]
    fn replacing_one_source_leaves_other_sources_alone() {
        let (_dir, mut store) = temp_store();
        store
            .replace_edges_many(&[("note:b.md".to_owned(), vec![edge(None, "from-b")])])
            .expect("b rows");
        store
            .replace_edges_many(&[("note:a.md".to_owned(), vec![edge(None, "from-a")])])
            .expect("a rows");
        let rows = store.all_graph_links().expect("read");
        let mut keys: Vec<&str> = rows
            .iter()
            .map(|row| row.from_source_key.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["note:a.md", "note:b.md"],
            "each source keeps its own rows"
        );
    }

    #[test]
    fn a_row_with_an_unknown_spelling_is_an_error_not_a_panic() {
        let (_dir, mut store) = temp_store();
        store
            .replace_edges_many(&[("note:a.md".to_owned(), vec![edge(None, "x")])])
            .expect("write");
        // The API cannot write this state (the enums are closed); a future
        // build can. Raw SQL is the only way to stand it up.
        store
            .connection
            .execute(
                "UPDATE graph_links SET edge_kind = 'kind_from_the_future'",
                [],
            )
            .expect("mutate");
        let err = store.all_graph_links().expect_err("typed read refuses");
        assert!(
            matches!(
                &err,
                StoreError::UnknownGraphSpelling { column, value }
                    if *column == "edge_kind" && value == "kind_from_the_future"
            ),
            "the reader names the column and the spelling: {err:?}"
        );
    }
}
