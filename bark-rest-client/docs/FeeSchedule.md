# FeeSchedule

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**board** | [**models::BoardFees**](BoardFees.md) |  | 
**expiry_payout** | Option<[**models::ExpiryPayoutFees**](ExpiryPayoutFees.md)> | The fee of an expiry payout. Present only when the Ark server is in expiry payout mode. | [optional]
**intra_ark** | Option<[**models::IntraArkFees**](IntraArkFees.md)> | The Lightning fees of a payment between two wallets of this Ark server. Absent when such a payment pays `lightning_send` and `lightning_receive`. | [optional]
**lightning_receive** | [**models::LightningReceiveFees**](LightningReceiveFees.md) |  | 
**lightning_send** | [**models::LightningSendFees**](LightningSendFees.md) |  | 
**offboard** | [**models::OffboardFees**](OffboardFees.md) |  | 
**refresh** | [**models::RefreshFees**](RefreshFees.md) |  | 

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


