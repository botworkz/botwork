use sea_orm::{DatabaseConnection, MockDatabase};

pub(crate) fn mock_db_connection(mock: MockDatabase) -> DatabaseConnection {
    mock.into_connection()
}
