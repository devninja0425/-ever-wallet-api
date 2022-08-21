use axum::extract::Path;
use axum::{Extension, Json};

use crate::axum_api::controllers::*;
use crate::axum_api::requests::*;
use crate::axum_api::responses::*;
use crate::axum_api::*;
use crate::models::*;

pub async fn post_address_create(
    Json(req): Json<CreateAddressRequest>,
    Extension(ctx): Extension<Arc<ApiContext>>,
    IdExtractor(service_id): IdExtractor,
) -> Result<Json<AddressResponse>> {
    let address = ctx
        .ton_service
        .create_address(&service_id, req.into())
        .await
        .map(From::from);

    Ok(Json(AddressResponse::from(address)))
}

pub async fn post_address_check(
    Json(req): Json<AddressCheckRequest>,
    Extension(ctx): Extension<Arc<ApiContext>>,
) -> Result<Json<CheckedAddressResponse>> {
    let address = ctx
        .ton_service
        .check_address(req.address)
        .await
        .map(AddressValidResponse::new);

    Ok(Json(CheckedAddressResponse::from(address)))
}

pub async fn get_address_balance(
    Path(address): Path<Address>,
    Extension(ctx): Extension<Arc<ApiContext>>,
    IdExtractor(service_id): IdExtractor,
) -> Result<Json<AddressBalanceResponse>> {
    let address = ctx
        .ton_service
        .get_address_balance(&service_id, address)
        .await
        .map(|(a, b)| AddressBalanceDataResponse::new(a, b));

    Ok(Json(AddressBalanceResponse::from(address)))
}

pub async fn get_address_info(
    Path(address): Path<Address>,
    Extension(ctx): Extension<Arc<ApiContext>>,
    IdExtractor(service_id): IdExtractor,
) -> Result<Json<AddressInfoResponse>> {
    let address = ctx
        .ton_service
        .get_address_info(&service_id, address)
        .await
        .map(AddressInfoDataResponse::new);

    Ok(Json(AddressInfoResponse::from(address)))
}
