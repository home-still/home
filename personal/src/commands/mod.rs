pub mod add;
pub mod config;
pub mod delete;
pub mod list;
pub mod read;
pub mod reindex;
pub mod search;

use crate::cli::PersonalCmd;
use crate::error::Result;

pub async fn dispatch(cmd: PersonalCmd) -> Result<()> {
    match cmd {
        PersonalCmd::Add {
            file,
            category,
            title,
            force,
        } => add::run(file, category, title, force).await,
        PersonalCmd::List { category, limit } => list::run(category, limit).await,
        PersonalCmd::Search {
            query,
            category,
            limit,
        } => search::run(query, category, limit).await,
        PersonalCmd::Read { stem } => read::run(stem).await,
        PersonalCmd::Delete { stem } => delete::run(stem).await,
        PersonalCmd::Reindex { stem } => reindex::run(stem).await,
        PersonalCmd::Config { action } => config::run(action).await,
    }
}
