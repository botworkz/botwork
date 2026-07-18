use std::sync::Arc;

use sea_orm::{DatabaseConnection, MockDatabase};

use crate::{AppState, ControlPlaneClient, SecretStoreClient, SessionBrokerClient};

pub(crate) fn app_state_with_db(db: DatabaseConnection) -> AppState {
    AppState {
        db: Arc::new(db),
        control_plane: ControlPlaneClient::disabled(),
        secret_store: SecretStoreClient::disabled(),
        session_broker: SessionBrokerClient::disabled(),
    }
}

pub(crate) fn app_state_with_mock_db(mock: MockDatabase) -> AppState {
    app_state_with_db(mock.into_connection())
}
