use std::sync::Arc;

use sea_orm::{DatabaseConnection, MockDatabase};

use crate::store::mock::MockApiStore;
use crate::store::sea_orm_impl::SeaOrmApiStore;
use crate::{AppState, ControlPlaneClient, SecretStoreClient, SessionBrokerClient};

pub(crate) fn app_state_with_db(db: DatabaseConnection) -> AppState {
    let db = Arc::new(db);
    AppState {
        store: Arc::new(SeaOrmApiStore::new(db.clone())),
        db,
        control_plane: ControlPlaneClient::disabled(),
        secret_store: SecretStoreClient::disabled(),
        session_broker: SessionBrokerClient::disabled(),
    }
}

pub(crate) fn app_state_with_mock_db(mock: MockDatabase) -> AppState {
    app_state_with_db(mock.into_connection())
}

pub(crate) fn app_state_with_mock_store(store: MockApiStore) -> AppState {
    AppState {
        db: Arc::new(MockDatabase::new(sea_orm::DatabaseBackend::Postgres).into_connection()),
        store: Arc::new(store),
        control_plane: ControlPlaneClient::disabled(),
        secret_store: SecretStoreClient::disabled(),
        session_broker: SessionBrokerClient::disabled(),
    }
}
