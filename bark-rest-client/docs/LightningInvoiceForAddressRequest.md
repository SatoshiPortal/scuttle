# LightningInvoiceForAddressRequest

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**address** | **String** | Ark address that will receive the claimed VTXO. | 
**amount_sat** | **i64** | The amount to create invoice for (in satoshis). | 
**ark_address_hint** | Option<**bool**> | Put `address` in the route hints of the invoice, so a wallet on the same Ark server can pay it with an arkoor straight to `address`. The Ark server must allow route hints. The receive is then in `awaiting-delivery` until the server reports the arkoor delivered to the mailbox of `address`, and then settled. | [optional]
**description** | Option<**String**> | Optional description embedded in the invoice as its memo. | [optional]
**expiry_secs** | Option<**i64**> | Optional number of seconds the invoice can be paid for. Defaults to, and cannot exceed, the Ark server's invoice expiry. | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


