//! Breez SDK Spark Lightning Backend Implementation
//!
//! This implementation uses the Breez SDK Spark to provide Lightning payment functionality
//! for the CDK payment processor.

use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use breez_sdk_spark::{
    BreezSdk, Config, ConnectRequest, Network, ReceivePaymentMethod, ReceivePaymentRequest, Seed,
    WaitForPaymentIdentifier, WaitForPaymentRequest,
};
use cdk_common::bitcoin::hashes::Hash;
use cdk_common::nuts::{CurrencyUnit, MeltQuoteState};
use cdk_common::payment::{
    CreateIncomingPaymentResponse, Event, IncomingPaymentOptions, MakePaymentResponse, MintPayment,
    OutgoingPaymentOptions, PaymentIdentifier, PaymentQuoteResponse, WaitPaymentResponse,
};
use cdk_common::Bolt11Invoice;
use futures_core::Stream;

use crate::database::QuoteDatabase;
use crate::settings::BackendConfig;

/// Breez SDK Spark backend implementation
pub struct BreezBackend {
    /// The Breez SDK instance
    sdk: Arc<BreezSdk>,
    /// Flag to track if we're actively waiting for invoice payments
    wait_invoice_active: Arc<AtomicBool>,
    /// Database for storing quote-to-payment mappings
    db: QuoteDatabase,
}

impl BreezBackend {
    /// Store a mint quote mapping (payment hash -> payment request)
    fn store_mint_quote(
        &self,
        payment_hash: &str,
        payment_request: &str,
    ) -> Result<(), cdk_common::payment::Error> {
        self.db
            .insert_mint_quote(payment_hash, payment_request)
            .map_err(|e| cdk_common::payment::Error::Custom(e.to_string()))
    }

    /// Get the payment request for a mint quote by payment hash
    fn get_mint_quote(
        &self,
        payment_hash: &str,
    ) -> Result<Option<String>, cdk_common::payment::Error> {
        self.db
            .get_mint_quote(payment_hash)
            .map_err(|e| cdk_common::payment::Error::Custom(e.to_string()))
    }

    /// Store a melt quote mapping (payment hash -> payment request)
    fn store_melt_quote(
        &self,
        payment_hash: &str,
        payment_request: &str,
    ) -> Result<(), cdk_common::payment::Error> {
        self.db
            .insert_melt_quote(payment_hash, payment_request)
            .map_err(|e| cdk_common::payment::Error::Custom(e.to_string()))
    }

    /// Get the payment request for a melt quote by payment hash
    fn get_melt_quote(
        &self,
        payment_hash: &str,
    ) -> Result<Option<String>, cdk_common::payment::Error> {
        self.db
            .get_melt_quote(payment_hash)
            .map_err(|e| cdk_common::payment::Error::Custom(e.to_string()))
    }

    /// Create a new Breez backend instance
    ///
    /// Initializes the Breez SDK with the provided configuration
    pub async fn new(config: BackendConfig) -> anyhow::Result<Self> {
        // Validate configuration
        if config.api_key.is_empty() {
            anyhow::bail!("Breez API key is required");
        }
        if config.mnemonic.is_empty() {
            anyhow::bail!("Mnemonic seed is required");
        }

        tracing::info!(
            "Initializing Breez backend with working_dir: {}",
            config.working_dir
        );

        // Create SDK configuration
        let sdk_config = Config {
            api_key: Some(config.api_key.clone()),
            network: Network::Mainnet,
            sync_interval_secs: 600,
            max_deposit_claim_fee: None,
            lnurl_domain: None,
            prefer_spark_over_lightning: true,
            external_input_parsers: None,
            use_default_external_input_parsers: true,
        };

        tracing::debug!("SDK config - network: Mainnet, sync_interval: 600s");

        // Create seed from mnemonic
        let seed = Seed::Mnemonic {
            mnemonic: config.mnemonic.clone(),
            passphrase: config.passphrase.clone(),
        };

        tracing::debug!("Seed created from mnemonic");

        // Create connect request
        let connect_request = ConnectRequest {
            config: sdk_config,
            seed,
            storage_dir: config.storage_dir(),
        };

        // Connect to Breez SDK
        tracing::info!("Connecting to Breez SDK...");
        let sdk = breez_sdk_spark::connect(connect_request)
            .await
            .map_err(|e| {
                tracing::error!("Failed to connect to Breez SDK: {:?}", e);
                anyhow::anyhow!("Breez SDK connection failed: {:?}", e)
            })?;
        tracing::info!("Successfully connected to Breez SDK");

        // Get SDK info to verify connection
        match sdk
            .get_info(breez_sdk_spark::GetInfoRequest {
                ensure_synced: None,
            })
            .await
        {
            Ok(info) => {
                tracing::debug!("SDK node info - balance: {} sats", info.balance_sats);
            }
            Err(e) => {
                tracing::warn!("Could not retrieve node info: {:?}", e);
            }
        }

        // Initialize database
        let db_path = config.db_path();
        tracing::info!("Initializing database at: {}", db_path);
        let db = QuoteDatabase::new(&db_path)?;

        Ok(Self {
            sdk: Arc::new(sdk),
            wait_invoice_active: Arc::new(AtomicBool::new(false)),
            db,
        })
    }
}

#[async_trait]
impl MintPayment for BreezBackend {
    type Err = cdk_common::payment::Error;

    /// Get backend settings - returns capabilities and supported features
    async fn get_settings(&self) -> Result<serde_json::Value, Self::Err> {
        // Breez SDK Spark supports BOLT11 invoices and Spark payments
        Ok(serde_json::json!({
            "bolt11": true,
            "bolt12": false,
            "mpp": true,
            "amp": false,
            "unit": "sat",
            "spark": true,
            "invoice_description": false,
            "amountless": false
        }))
    }

    /// Create an incoming payment request (invoice)
    async fn create_incoming_payment_request(
        &self,
        _unit: &CurrencyUnit,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        tracing::info!("Creating incoming payment request");
        match options {
            IncomingPaymentOptions::Bolt11(opts) => {
                let description = opts
                    .description
                    .clone()
                    .unwrap_or_else(|| "Payment".to_string());
                let amount_sats = if opts.amount > cdk_common::Amount::from(0) {
                    Some(Into::<u64>::into(opts.amount))
                } else {
                    None
                };

                tracing::debug!(
                    "BOLT11 invoice request - description: '{}', amount_sats: {:?}",
                    description,
                    amount_sats
                );

                let request = ReceivePaymentRequest {
                    payment_method: ReceivePaymentMethod::Bolt11Invoice {
                        description: description.clone(),
                        amount_sats,
                    },
                };

                tracing::debug!("Calling Breez SDK receive_payment");
                let response = self.sdk.receive_payment(request).await.map_err(|e| {
                    tracing::error!("Breez SDK receive_payment failed: {:?}", e);
                    cdk_common::payment::Error::Lightning(Box::new(e))
                })?;

                tracing::info!("Successfully created invoice: {}", response.payment_request);

                let invoice = Bolt11Invoice::from_str(&response.payment_request)?;
                let payment_hash = invoice.payment_hash();
                let payment_hash_hex = hex::encode(payment_hash.as_byte_array());
                let payment_identifier =
                    PaymentIdentifier::PaymentHash(payment_hash.to_byte_array());

                tracing::debug!("Payment identifier created: {:?}", payment_identifier);

                // Store the mapping: payment_hash -> payment_request
                self.store_mint_quote(&payment_hash_hex, &response.payment_request)?;
                tracing::debug!(
                    "Stored mint quote mapping: {} -> {}",
                    payment_hash_hex,
                    response.payment_request
                );

                Ok(CreateIncomingPaymentResponse {
                    request_lookup_id: payment_identifier,
                    request: response.payment_request,
                    expiry: None,
                })
            }
            _ => {
                tracing::error!("Unsupported payment option requested: {:?}", options);
                Err(cdk_common::payment::Error::UnsupportedPaymentOption)
            }
        }
    }

    /// Get a payment quote (fee estimation for outgoing payment)
    async fn get_payment_quote(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        match options {
            OutgoingPaymentOptions::Bolt11(opts) => {
                use breez_sdk_spark::PrepareSendPaymentRequest;
                use cdk_common::amount::Amount;

                let bolt11_str = opts.bolt11.to_string();
                let prepare_request = PrepareSendPaymentRequest {
                    payment_request: bolt11_str.clone(),
                    amount: None,
                    token_identifier: None,
                };

                let prepare_response = self
                    .sdk
                    .prepare_send_payment(prepare_request)
                    .await
                    .map_err(|e| cdk_common::payment::Error::Lightning(Box::new(e)))?;

                // Calculate fee from payment method
                let fee = match &prepare_response.payment_method {
                    breez_sdk_spark::SendPaymentMethod::Bolt11Invoice {
                        spark_transfer_fee_sats,
                        lightning_fee_sats,
                        ..
                    } => {
                        let total_fee = spark_transfer_fee_sats.unwrap_or(0) + lightning_fee_sats;
                        Amount::from(total_fee)
                    }
                    _ => Amount::from(0),
                };

                let amount = Amount::from(prepare_response.amount as u64);

                // Extract payment hash from the invoice and store mapping
                let invoice = Bolt11Invoice::from_str(&bolt11_str)?;
                let payment_hash = invoice.payment_hash();
                let payment_hash_hex = hex::encode(payment_hash.as_byte_array());
                let payment_identifier =
                    PaymentIdentifier::PaymentHash(payment_hash.to_byte_array());

                // Store the mapping: payment_hash -> payment_request
                self.store_melt_quote(&payment_hash_hex, &bolt11_str)?;
                tracing::debug!(
                    "Stored melt quote mapping: {} -> {}",
                    payment_hash_hex,
                    bolt11_str
                );

                Ok(PaymentQuoteResponse {
                    request_lookup_id: Some(payment_identifier),
                    amount,
                    fee,
                    unit: unit.clone(),
                    state: MeltQuoteState::Unpaid,
                })
            }
            _ => Err(cdk_common::payment::Error::UnsupportedPaymentOption),
        }
    }

    /// Make an outgoing payment
    async fn make_payment(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        if *unit != CurrencyUnit::Sat {
            panic!();
        }
        match options {
            OutgoingPaymentOptions::Bolt11(opts) => {
                use breez_sdk_spark::{PrepareSendPaymentRequest, SendPaymentRequest};
                use cdk_common::amount::Amount;

                // First, prepare the payment to get fee information
                let bolt11_str = opts.bolt11.to_string();
                tracing::info!("Making payment for invoice: {}", bolt11_str);

                let prepare_request = PrepareSendPaymentRequest {
                    payment_request: bolt11_str.clone(),
                    amount: None,
                    token_identifier: None,
                };

                let prepare_response = self
                    .sdk
                    .prepare_send_payment(prepare_request)
                    .await
                    .map_err(|e| {
                        tracing::error!("Failed to prepare payment: {:?}", e);
                        cdk_common::payment::Error::Lightning(Box::new(e))
                    })?;

                tracing::debug!(
                    "Payment prepared - amount: {} sats",
                    prepare_response.amount
                );

                // Now send the payment
                let send_request = SendPaymentRequest {
                    prepare_response,
                    options: None,
                };

                let send_response = self.sdk.send_payment(send_request).await.map_err(|e| {
                    tracing::error!("Failed to send payment: {:?}", e);
                    cdk_common::payment::Error::Lightning(Box::new(e))
                })?;

                let payment_amount = send_response.payment.amount;
                let payment_fees = send_response.payment.fees;
                let total_spent = Amount::from((payment_amount + payment_fees) as u64);

                tracing::info!(
                    "Payment successful - amount: {} sats, fees: {} sats, total: {} {}, payment_id: {}",
                    payment_amount,
                    payment_fees,
                    total_spent,
                    unit.to_string(),
                    send_response.payment.id
                );

                // Extract payment hash from the invoice
                let invoice = Bolt11Invoice::from_str(&bolt11_str)?;
                let payment_hash = invoice.payment_hash();
                let payment_hash_hex = hex::encode(payment_hash.as_byte_array());
                let payment_identifier =
                    PaymentIdentifier::PaymentHash(payment_hash.to_byte_array());

                tracing::debug!("Payment hash: {}", payment_hash_hex);
                tracing::info!("Payment total spent: {}", total_spent);

                Ok(MakePaymentResponse {
                    payment_lookup_id: payment_identifier,
                    payment_proof: None,
                    status: MeltQuoteState::Paid,
                    total_spent,
                    unit: CurrencyUnit::Sat,
                })
            }
            _ => Err(cdk_common::payment::Error::UnsupportedPaymentOption),
        }
    }

    /// Wait for payment events - returns a stream of incoming payment events
    async fn wait_payment_event(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = Event> + Send>>, Self::Err> {
        use breez_sdk_spark::{EventListener, SdkEvent};
        use tokio::sync::mpsc;
        use tokio_stream::wrappers::ReceiverStream;

        self.wait_invoice_active.store(true, Ordering::Relaxed);

        let (tx, rx) = mpsc::channel(100);

        // Create event listener
        struct PaymentEventListener {
            sender: mpsc::Sender<Event>,
        }

        #[async_trait::async_trait]
        impl EventListener for PaymentEventListener {
            async fn on_event(&self, event: SdkEvent) {
                use breez_sdk_spark::PaymentDetails;
                use cdk_common::amount::Amount;

                if let SdkEvent::PaymentSucceeded { payment } = event {
                    // Extract payment hash from payment details
                    let payment_identifier = if let Some(PaymentDetails::Lightning {
                        ref payment_hash,
                        ..
                    }) = payment.details
                    {
                        // Convert hex string to bytes
                        if let Ok(hash_bytes) = hex::decode(payment_hash) {
                            if let Ok(hash_array) = hash_bytes.try_into() {
                                PaymentIdentifier::PaymentHash(hash_array)
                            } else {
                                tracing::warn!("Payment hash wrong length: {}", payment_hash);
                                // Fallback to payment ID bytes
                                PaymentIdentifier::PaymentHash(
                                    payment.id.as_bytes()[..32].try_into().unwrap_or([0; 32]),
                                )
                            }
                        } else {
                            tracing::warn!("Failed to decode payment hash: {}", payment_hash);
                            // Fallback to payment ID bytes
                            PaymentIdentifier::PaymentHash(
                                payment.id.as_bytes()[..32].try_into().unwrap_or([0; 32]),
                            )
                        }
                    } else {
                        tracing::warn!(
                            "No Lightning payment details found for payment: {}",
                            payment.id
                        );
                        // Fallback to payment ID bytes
                        PaymentIdentifier::PaymentHash(
                            payment.id.as_bytes()[..32].try_into().unwrap_or([0; 32]),
                        )
                    };

                    // Convert to CDK event
                    let cdk_event = Event::PaymentReceived(WaitPaymentResponse {
                        payment_id: payment.id.clone(),
                        payment_identifier,
                        payment_amount: Amount::from((payment.amount + payment.fees) as u64),
                        unit: CurrencyUnit::Sat,
                    });

                    let _ = self.sender.send(cdk_event).await;
                }
            }
        }

        let listener = Box::new(PaymentEventListener { sender: tx });

        let _listener_id = self.sdk.add_event_listener(listener).await;

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    /// Check if wait invoice is currently active
    fn is_wait_invoice_active(&self) -> bool {
        self.wait_invoice_active.load(Ordering::Relaxed)
    }

    /// Cancel waiting for invoice payments
    fn cancel_wait_invoice(&self) {
        self.wait_invoice_active.store(false, Ordering::Relaxed);
    }

    /// Check the status of an incoming payment
    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        tracing::info!(
            "Checking incoming payment status for identifier: {:?}",
            payment_identifier
        );

        // Convert payment identifier to hex string
        let payment_hash_hex = match payment_identifier {
            PaymentIdentifier::PaymentHash(hash) => hex::encode(hash),
            _ => payment_identifier.to_string(),
        };
        tracing::debug!("Payment hash (hex): {}", payment_hash_hex);

        // Get the stored payment request from the database
        let payment_request = match self.get_mint_quote(&payment_hash_hex)? {
            Some(req) => {
                tracing::debug!("Found stored payment request: {}", req);
                req
            }
            None => {
                tracing::warn!(
                    "No stored payment request found for hash: {}",
                    payment_hash_hex
                );
                return Ok(vec![]);
            }
        };

        // Use wait_for_payment to check the status
        let request = WaitForPaymentRequest {
            identifier: WaitForPaymentIdentifier::PaymentRequest(payment_request.clone()),
        };

        use cdk_common::amount::Amount;

        tracing::debug!("Calling Breez SDK wait_for_payment");
        match self.sdk.wait_for_payment(request).await {
            Ok(response) => {
                tracing::info!(
                    "Payment found - id: {}, amount: {}, fees: {}",
                    response.payment.id,
                    response.payment.amount,
                    response.payment.fees
                );

                let payment_response = WaitPaymentResponse {
                    payment_id: response.payment.id.clone(),
                    payment_identifier: PaymentIdentifier::PaymentHash(
                        response.payment.id.as_bytes()[..32]
                            .try_into()
                            .unwrap_or([0; 32]),
                    ),
                    payment_amount: Amount::from(
                        (response.payment.amount + response.payment.fees) as u64,
                    ),
                    unit: CurrencyUnit::Sat,
                };

                tracing::debug!("Returning payment response: {:?}", payment_response);
                Ok(vec![payment_response])
            }
            Err(e) => {
                tracing::warn!(
                    "Payment not found or error checking status for {}: {:?}",
                    payment_request,
                    e
                );
                Ok(vec![])
            }
        }
    }

    /// Check the status of an outgoing payment
    async fn check_outgoing_payment(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        use breez_sdk_spark::{ListPaymentsRequest, PaymentStatus, PaymentType};
        use cdk_common::amount::Amount;

        // Convert payment identifier to hex string
        let payment_hash_hex = match payment_identifier {
            PaymentIdentifier::PaymentHash(hash) => hex::encode(hash),
            _ => payment_identifier.to_string(),
        };
        tracing::debug!(
            "Checking outgoing payment with hash (hex): {}",
            payment_hash_hex
        );

        // Get the stored payment request from the database
        let payment_request = match self.get_melt_quote(&payment_hash_hex)? {
            Some(req) => {
                tracing::debug!("Found stored payment request: {}", req);
                req
            }
            None => {
                tracing::warn!(
                    "No stored payment request found for hash: {}",
                    payment_hash_hex
                );
                return Err(cdk_common::payment::Error::Custom(
                    "Payment not found in database".to_string(),
                ));
            }
        };

        // List payments and find the matching one by payment request
        let request = ListPaymentsRequest {
            type_filter: Some(vec![PaymentType::Send]),
            ..Default::default()
        };

        let response = self
            .sdk
            .list_payments(request)
            .await
            .map_err(|e| cdk_common::payment::Error::Lightning(Box::new(e)))?;

        let payments = response.payments;

        // Find the payment by payment request (invoice)
        let payment = payments
            .into_iter()
            .find(|p| {
                // Compare invoice in payment details if available
                if let Some(breez_sdk_spark::PaymentDetails::Lightning { ref invoice, .. }) =
                    p.details
                {
                    invoice == &payment_request
                } else {
                    false
                }
            })
            .ok_or(cdk_common::payment::Error::Custom(
                "Payment not found".to_string(),
            ))?;

        let status = match payment.status {
            PaymentStatus::Completed => MeltQuoteState::Paid,
            PaymentStatus::Failed => MeltQuoteState::Unpaid,
            PaymentStatus::Pending => MeltQuoteState::Pending,
        };

        Ok(MakePaymentResponse {
            payment_lookup_id: payment_identifier.clone(),
            payment_proof: None,
            status,
            total_spent: Amount::from((payment.amount + payment.fees) as u64),
            unit: CurrencyUnit::Sat,
        })
    }
}
