//! SeaORM Entity. Generated by sea-orm-codegen 0.10.0

use super::sea_orm_active_enums::Method;
use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "revert_logs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: u64,
    pub rpc_key_id: u64,
    pub timestamp: DateTimeUtc,
    pub method: Method,
    pub to: Vec<u8>,
    #[sea_orm(column_type = "Text", nullable)]
    pub call_data: Option<String>,
    pub chain_id: u64,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::rpc_keys::Entity",
        from = "Column::RpcKeyId",
        to = "super::rpc_keys::Column::Id",
        on_update = "NoAction",
        on_delete = "NoAction"
    )]
    UserKeys,
}

impl Related<super::rpc_keys::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::UserKeys.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
