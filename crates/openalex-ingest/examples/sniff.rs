use openalex_ingest::model::Work;
use std::io::BufRead;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        "/home/ladvien/data/academic_papers/openalex-snapshot/data/works/updated_date=2016-06-24/part_0000.jsonl".to_string()
    });
    let f = std::fs::File::open(&path).unwrap();
    let buf = std::io::BufReader::new(f);
    let mut errors_shown = 0;
    let mut total = 0;
    let mut errs = 0;
    for (i, line) in buf.lines().enumerate() {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        total += 1;
        if let Err(e) = serde_json::from_str::<Work>(&line) {
            errs += 1;
            if errors_shown < 5 {
                println!("--- line {} error: {} ---", i + 1, e);
                println!("{}", &line.chars().take(500).collect::<String>());
                errors_shown += 1;
            }
        }
        if i >= 100 {
            break;
        }
    }
    println!("\n{}/{} parse errors", errs, total);
}
