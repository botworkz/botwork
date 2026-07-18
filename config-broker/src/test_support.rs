use std::sync::Arc;

use sea_orm::{DatabaseConnection, MockDatabase};

use crate::AppState;

pub(crate) fn app_state_with_db(db: DatabaseConnection) -> AppState {
    AppState { db: Arc::new(db) }
}

pub(crate) fn app_state_with_mock_db(mock: MockDatabase) -> AppState {
    app_state_with_db(mock.into_connection())
}
