//! End-to-end behavior of the works / authors / simple-entity loaders on
//! synthetic snapshots in temp dirs: failure injection, commit ordering,
//! resume, tolerance policy, read-only access. Never touches real data.

use openalex_ingest::{LoadOptions, OpenAlexDb, SeenSet, SimpleEntity};
use std::path::{Path, PathBuf};

const NEW: &str = "updated_date=2024-02-01";
const OLD: &str = "updated_date=2024-01-01";

fn work(id: u64, title: &str, topic: Option<&str>) -> String {
    let topics = match topic {
        Some(t) => format!(r#"[{{"id":"https://openalex.org/{t}","score":0.5}}]"#),
        None => "[]".to_string(),
    };
    format!(
        r#"{{"id":"https://openalex.org/W{id}","doi":"https://doi.org/10.1/w{id}","title":"{title}","publication_year":2020,"publication_date":"2020-01-01","authorships":[{{"author":{{"id":"https://openalex.org/A{id}"}},"author_position":"first"}}],"topics":{topics},"concepts":[{{"id":"https://openalex.org/C1","score":0.4}}],"referenced_works":["https://openalex.org/W99"]}}"#
    )
}

fn write_file(root: &Path, entity: &str, partition: &str, file: &str, lines: &[String]) {
    let dir = root.join(entity).join(partition);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file), lines.join("\n") + "\n").unwrap();
}

fn write_partition(root: &Path, partition: &str, lines: &[String]) {
    write_file(root, "works", partition, "part_0000.jsonl", lines);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    snap: PathBuf,
    seen_path: PathBuf,
    db: OpenAlexDb,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let snap = tmp.path().join("snap");
    let db = OpenAlexDb::open(&tmp.path().join("oa.duckdb")).unwrap();
    Fixture {
        seen_path: tmp.path().join("seen_set.bin"),
        _tmp: tmp,
        snap,
        db,
    }
}

impl Fixture {
    fn load(&self) -> anyhow::Result<openalex_ingest::EntityStats> {
        self.load_with(&LoadOptions::default())
    }

    fn load_with(&self, opts: &LoadOptions) -> anyhow::Result<openalex_ingest::EntityStats> {
        self.db.load_works(&self.snap, self.seen_path.clone(), opts)
    }

    fn count(&self, table: &str) -> u64 {
        self.db
            .raw()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn titles(&self) -> Vec<(String, String)> {
        self.db
            .raw()
            .prepare("SELECT openalex_id, title FROM works ORDER BY 1")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn log(&self) -> Vec<(String, String, u64, u64)> {
        self.db
            .raw()
            .prepare(
                "SELECT partition, status, rows, parse_errors FROM _ingest_log \
                 WHERE entity = 'works' ORDER BY 1",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// IDs a fresh `SeenSet::open` would hold (works rows + checkpoint).
    fn seen_len(&self) -> usize {
        SeenSet::open(self.db.raw(), self.seen_path.clone())
            .unwrap()
            .len()
    }

    /// Make the next merge of a partition containing W1 fail AFTER the
    /// `works` INSERT: a stray `work_topics` row collides with W1's topic.
    fn inject_topic_fault(&self) {
        self.db
            .raw()
            .execute_batch("INSERT INTO work_topics VALUES ('W1','T1',0.1)")
            .unwrap();
    }

    fn remove_topic_fault(&self) {
        self.db
            .raw()
            .execute_batch("DELETE FROM work_topics WHERE work_id = 'W1'")
            .unwrap();
    }
}

fn two_partition_snapshot(f: &Fixture) {
    write_partition(
        &f.snap,
        NEW,
        &[work(1, "new1", Some("T1")), work(2, "new2", None)],
    );
    write_partition(
        &f.snap,
        OLD,
        &[work(2, "old2", None), work(3, "old3", None)],
    );
}

// ---- RA-8 / RA-34 / RA-35 ------------------------------------------------

#[test]
fn failed_partition_aborts_the_load_and_leaves_no_trace() {
    let f = fixture();
    two_partition_snapshot(&f);
    f.inject_topic_fault();

    let err = f.load().expect_err("a failed partition must fail the load");
    assert!(
        format!("{err:#}").contains(NEW),
        "error names the partition: {err:#}"
    );

    // One transaction: `works` and the edges inserted before the failing
    // statement rolled back with it.
    assert_eq!(f.count("works"), 0);
    assert_eq!(f.count("work_authorships"), 0);
    assert_eq!(f.count("work_concepts"), 0);
    assert_eq!(f.count("work_references"), 0);
    // The failure is recorded; the older partition was NOT loaded past it.
    assert_eq!(f.log(), [(NEW.to_string(), "error".to_string(), 0, 0)]);
    // Neither the in-RAM set nor any checkpoint holds the failed IDs.
    assert_eq!(f.seen_len(), 0);
    assert!(
        !f.seen_path.exists() || std::fs::metadata(&f.seen_path).unwrap().len() == 0,
        "no checkpoint with uncommitted IDs"
    );
}

#[test]
fn rerun_after_the_fault_is_removed_loads_exactly_the_missing_rows() {
    let f = fixture();
    two_partition_snapshot(&f);
    f.inject_topic_fault();
    f.load().expect_err("first run fails");

    f.remove_topic_fault();
    let stats = f.load().expect("re-run after the fault is gone succeeds");
    assert_eq!(stats.partitions_loaded, 2);
    assert_eq!(stats.rows_inserted, 3);

    // Newest version of W2 wins; every work has its edges, once.
    assert_eq!(
        f.titles(),
        [
            ("W1".to_string(), "new1".to_string()),
            ("W2".to_string(), "new2".to_string()),
            ("W3".to_string(), "old3".to_string()),
        ]
    );
    assert_eq!(f.count("work_authorships"), 3);
    assert_eq!(f.count("work_concepts"), 3);
    assert_eq!(f.count("work_references"), 3);
    assert_eq!(f.count("work_topics"), 1);
    assert_eq!(
        f.log(),
        [
            (OLD.to_string(), "ok".to_string(), 1, 0),
            (NEW.to_string(), "ok".to_string(), 2, 0),
        ]
    );
    assert_eq!(f.seen_len(), 3);
    // And a further run is a no-op.
    let again = f.load().unwrap();
    assert_eq!((again.partitions_loaded, again.rows_inserted), (0, 0));
}

#[test]
fn partly_loaded_partition_resumes_without_duplicating_committed_files() {
    let f = fixture();
    write_file(
        &f.snap,
        "works",
        NEW,
        "part_0000.jsonl",
        &[work(2, "new2", None)],
    );
    write_file(
        &f.snap,
        "works",
        NEW,
        "part_0001.jsonl",
        &[work(1, "new1", Some("T1"))],
    );
    f.inject_topic_fault();

    f.load().expect_err("second file of the partition fails");
    // File 1 committed on its own; file 2 rolled back.
    assert_eq!(f.titles(), [("W2".to_string(), "new2".to_string())]);
    assert_eq!(f.count("work_authorships"), 1);
    assert_eq!(f.log()[0].1, "error");
    assert_eq!(
        f.log()[0].2,
        1,
        "the stamp reports the rows that did commit"
    );

    f.remove_topic_fault();
    let stats = f.load().unwrap();
    assert_eq!(
        stats.rows_inserted, 1,
        "only the missing file's work is loaded"
    );
    assert_eq!(f.count("works"), 2);
    assert_eq!(
        f.count("work_authorships"),
        2,
        "W2's authorship is not duplicated"
    );
    assert_eq!(f.count("work_concepts"), 2);
}

#[test]
fn works_committed_before_a_missing_checkpoint_still_dedupe_older_partitions() {
    // Crash window: the newer partition committed and was logged, the
    // process died before the checkpoint was written. The older partition
    // must still skip W2.
    let f = fixture();
    write_partition(
        &f.snap,
        NEW,
        &[work(1, "new1", None), work(2, "new2", None)],
    );
    f.load().unwrap();
    std::fs::remove_file(&f.seen_path).unwrap();

    write_partition(
        &f.snap,
        OLD,
        &[work(2, "old2", None), work(3, "old3", None)],
    );
    let stats = f.load().unwrap();
    assert_eq!(stats.rows_inserted, 1);
    assert_eq!(
        f.titles(),
        [
            ("W1".to_string(), "new1".to_string()),
            ("W2".to_string(), "new2".to_string()),
            ("W3".to_string(), "old3".to_string()),
        ]
    );
}

#[test]
fn a_checkpoint_ahead_of_the_database_refuses_the_load() {
    // What the pre-fix loader left behind: IDs marked before their INSERT.
    let f = fixture();
    write_partition(&f.snap, NEW, &[work(1, "new1", None)]);
    let ahead = f.seen_path.clone();
    std::fs::write(&ahead, [1u8, 0, 0, 0, 0, 0, 0, 0]).unwrap(); // W1 "seen", never inserted

    assert!(f.load().is_err(), "must not silently skip W1 for good");
    assert_eq!(f.count("works"), 0);
    assert!(ahead.exists(), "the operator's file is left untouched");
}

#[test]
fn checkpoint_is_written_after_commit_and_matches_the_database() {
    let f = fixture();
    two_partition_snapshot(&f);
    f.load().unwrap();
    let bytes = std::fs::metadata(&f.seen_path).unwrap().len();
    assert_eq!(bytes, 3 * 8, "one 8-byte ID per committed work");
}

#[test]
fn duplicate_ids_inside_a_file_and_across_partitions_load_once() {
    let f = fixture();
    write_partition(
        &f.snap,
        NEW,
        &[work(1, "first", None), work(1, "second", None)],
    );
    write_partition(&f.snap, OLD, &[work(1, "older", None)]);
    let stats = f.load().unwrap();
    assert_eq!(stats.rows_inserted, 1);
    assert_eq!(f.titles(), [("W1".to_string(), "first".to_string())]);
}

// ---- RA-33 ---------------------------------------------------------------

#[test]
fn a_work_row_the_appender_refuses_fails_the_file_and_leaves_no_orphan_edges() {
    let f = fixture();
    let bad = work(1, "bad", None).replace(
        r#""publication_date":"2020-01-01""#,
        r#""publication_date":"not-a-date""#,
    );
    write_partition(&f.snap, NEW, &[work(2, "fine", None), bad]);

    let err = f
        .load()
        .expect_err("an unstageable work must fail the load");
    assert!(
        format!("{err:#}").contains("W1"),
        "error names the work: {err:#}"
    );
    assert_eq!(f.count("works"), 0);
    assert_eq!(f.count("work_authorships"), 0);
    assert_eq!(f.count("work_concepts"), 0);
    assert_eq!(f.count("work_references"), 0);
    assert_eq!(f.seen_len(), 0);
}

// ---- RA-35: tolerance policy --------------------------------------------

#[test]
fn unusable_records_within_the_budget_are_skipped_and_recorded() {
    let f = fixture();
    write_partition(
        &f.snap,
        NEW,
        &[
            work(1, "ok", None),
            "this is not json".to_string(),
            r#"{"id":"https://openalex.org/A7","title":"not a work id"}"#.to_string(),
        ],
    );
    let opts = LoadOptions {
        max_parse_errors_per_partition: 2,
    };
    let stats = f.load_with(&opts).unwrap();
    assert_eq!((stats.rows_inserted, stats.parse_errors), (1, 2));
    assert_eq!(f.log(), [(NEW.to_string(), "ok".to_string(), 1, 2)]);
}

#[test]
fn unusable_records_beyond_the_budget_fail_the_partition_without_merging() {
    let f = fixture();
    write_partition(
        &f.snap,
        NEW,
        &[
            work(1, "ok", None),
            "this is not json".to_string(),
            r#"{"id":"https://openalex.org/A7","title":"not a work id"}"#.to_string(),
        ],
    );
    let opts = LoadOptions {
        max_parse_errors_per_partition: 1,
    };
    f.load_with(&opts)
        .expect_err("2 unusable records > budget of 1");
    assert_eq!(
        f.count("works"),
        0,
        "nothing from the failing file is merged"
    );
    assert_eq!(f.log()[0].1, "error");
    assert_eq!(f.seen_len(), 0);
}

#[test]
fn parse_error_budget_is_per_partition_not_per_load() {
    let f = fixture();
    write_partition(&f.snap, NEW, &[work(1, "a", None), "bad".to_string()]);
    write_partition(&f.snap, OLD, &[work(2, "b", None), "bad".to_string()]);
    let opts = LoadOptions {
        max_parse_errors_per_partition: 1,
    };
    let stats = f.load_with(&opts).unwrap();
    assert_eq!((stats.partitions_loaded, stats.parse_errors), (2, 2));
}

#[test]
fn a_partition_without_data_files_fails_instead_of_logging_ok() {
    let f = fixture();
    let dir = f.snap.join("works").join(NEW);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("part_0000.gz.19B6a7d8"), "partial upload").unwrap();
    f.load().expect_err("no data file in the partition");
    assert_eq!(f.log()[0].1, "error");
}

#[test]
fn a_gz_partition_loads_like_a_jsonl_one() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let f = fixture();
    let dir = f.snap.join("works").join(NEW);
    std::fs::create_dir_all(&dir).unwrap();
    let mut enc = GzEncoder::new(
        std::fs::File::create(dir.join("part_0000.gz")).unwrap(),
        Compression::default(),
    );
    writeln!(enc, "{}", work(1, "from gz", None)).unwrap();
    enc.finish().unwrap();
    let stats = f.load().unwrap();
    assert_eq!(stats.rows_inserted, 1);
    assert_eq!(f.titles(), [("W1".to_string(), "from gz".to_string())]);
}

#[test]
fn a_truncated_gz_file_fails_the_partition() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let f = fixture();
    let dir = f.snap.join("works").join(NEW);
    std::fs::create_dir_all(&dir).unwrap();
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    for i in 1..=2000 {
        writeln!(enc, "{}", work(i, "t", None)).unwrap();
    }
    let gz = enc.finish().unwrap();
    std::fs::write(dir.join("part_0000.gz"), &gz[..gz.len() / 2]).unwrap();
    f.load()
        .expect_err("a truncated gz must not load as a short partition");
    assert_eq!(f.count("works"), 0);
    assert_eq!(f.log()[0].1, "error");
}

// ---- authors -------------------------------------------------------------

fn author(id: u64) -> String {
    format!(
        r#"{{"id":"https://openalex.org/A{id}","display_name":"Author {id}","works_count":1,"cited_by_count":2}}"#
    )
}

#[test]
fn authors_load_and_a_duplicate_key_fails_the_partition() {
    let f = fixture();
    write_file(
        &f.snap,
        "authors",
        NEW,
        "part_0000.jsonl",
        &[author(1), author(2)],
    );
    let opts = LoadOptions::default();
    let s = f.db.load_authors(&f.snap, &opts).unwrap();
    assert_eq!(s.rows_inserted, 2);
    assert_eq!(f.count("authors"), 2);

    // A second partition re-emitting author 2 violates the PK; that must be
    // an error and an `error` stamp, never a silent "ok".
    write_file(
        &f.snap,
        "authors",
        OLD,
        "part_0000.jsonl",
        &[author(3), author(2)],
    );
    f.db.load_authors(&f.snap, &opts)
        .expect_err("duplicate author id");
    let status: String =
        f.db.raw()
            .query_row(
                "SELECT status FROM _ingest_log WHERE entity='authors' AND partition=?",
                [OLD],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(status, "error");
}

#[test]
fn authors_partition_over_the_parse_budget_fails() {
    let f = fixture();
    write_file(
        &f.snap,
        "authors",
        NEW,
        "part_0000.jsonl",
        &[author(1), "bad".to_string(), "worse".to_string()],
    );
    let tight = LoadOptions {
        max_parse_errors_per_partition: 1,
    };
    f.db.load_authors(&f.snap, &tight)
        .expect_err("2 bad lines > 1");
    let loose = LoadOptions {
        max_parse_errors_per_partition: 2,
    };
    // The failed partition is retried (status 'error' is not 'ok'); the row
    // flushed by the failed attempt makes the retry fail on its duplicate
    // key, which is the documented non-atomic authors behavior.
    assert!(f.db.load_authors(&f.snap, &loose).is_err());
}

// ---- simple entities / SQL construction (RA-113) -------------------------

fn concept(id: u64) -> String {
    format!(
        r#"{{"id":"https://openalex.org/C{id}","display_name":"Concept {id}","level":0,"description":"d","wikidata":"https://www.wikidata.org/wiki/Q{id}","works_count":1,"cited_by_count":2,"ancestors":[{{"id":"https://openalex.org/C9","display_name":"Root","level":0}}]}}"#
    )
}

#[test]
fn simple_entity_loads_from_a_path_containing_a_quote() {
    let tmp = tempfile::tempdir().unwrap();
    let snap = tmp.path().join("o'brien \"snapshot\"");
    write_file(
        &snap,
        "concepts",
        NEW,
        "part_0000.jsonl",
        &[concept(1), concept(2)],
    );
    // The data dir (and so DuckDB's temp_directory) has a quote too.
    let db = OpenAlexDb::open(&tmp.path().join("it's").join("oa.duckdb")).unwrap();
    let s = db
        .load_simple_entity(SimpleEntity::Concepts, &snap)
        .unwrap();
    assert_eq!(s.rows_inserted, 2);
}

#[test]
fn simple_entity_rejects_glob_metacharacters_in_the_partition_path() {
    let tmp = tempfile::tempdir().unwrap();
    let snap = tmp.path().join("snap[1]");
    write_file(&snap, "concepts", NEW, "part_0000.jsonl", &[concept(1)]);
    let db = OpenAlexDb::open(&tmp.path().join("oa.duckdb")).unwrap();
    assert!(db
        .load_simple_entity(SimpleEntity::Concepts, &snap)
        .is_err());
    let n: u64 = db
        .raw()
        .query_row("SELECT COUNT(*) FROM concepts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn simple_entity_failure_is_stamped_and_returned() {
    let tmp = tempfile::tempdir().unwrap();
    let snap = tmp.path().join("snap");
    write_file(
        &snap,
        "concepts",
        NEW,
        "part_0000.jsonl",
        &["{not json".to_string()],
    );
    let db = OpenAlexDb::open(&tmp.path().join("oa.duckdb")).unwrap();
    assert!(db
        .load_simple_entity(SimpleEntity::Concepts, &snap)
        .is_err());
    let status: String = db
        .raw()
        .query_row(
            "SELECT status FROM _ingest_log WHERE entity='concepts'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "error");
}

// ---- RA-109 ---------------------------------------------------------------

#[test]
fn read_only_open_never_creates_the_file_and_rejects_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("data").join("oa.duckdb");
    assert!(OpenAlexDb::open_read_only(&missing).is_err());
    assert!(!missing.exists(), "read-only open must not create the file");
    assert!(!missing.parent().unwrap().exists(), "nor its directory");

    let path = tmp.path().join("oa.duckdb");
    OpenAlexDb::open(&path).unwrap(); // creates + applies the schema, then closes
    let ro = OpenAlexDb::open_read_only(&path).unwrap();
    assert_eq!(ro.row_counts().unwrap().len(), 15, "reads work");
    assert!(ro.raw().execute_batch("CREATE TABLE t(x INT)").is_err());
    assert!(ro
        .raw()
        .execute_batch("INSERT INTO works(openalex_id) VALUES ('W1')")
        .is_err());
    assert!(ro.raw().execute_batch("DROP TABLE works").is_err());
}
