//   Copyright 2023. The Tari Project
//
//   Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//   following conditions are met:
//
//   1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//   disclaimer.
//
//   2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//   following disclaimer in the documentation and/or other materials provided with the distribution.
//
//   3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//   products derived from this software without specific prior written permission.
//
//   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//   INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//   SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//   WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//   USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::{collections::BTreeMap, str::FromStr, sync::Arc};

use async_graphql::{Context, EmptyMutation, EmptySubscription, Object, Schema, SimpleObject};
use log::*;
use serde::{Deserialize, Serialize};
use tari_engine_types::substate::SubstateId;
use tari_template_lib::Hash;
use tari_transaction::TransactionId;

use crate::event_manager::EventManager;

const LOG_TARGET: &str = "tari::indexer::graphql::events";

#[derive(SimpleObject, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub substate_id: Option<String>,
    pub template_address: [u8; 32],
    pub tx_hash: [u8; 32],
    pub topic: String,
    pub payload: BTreeMap<String, String>,
}

impl Event {
    fn from_engine_event(event: tari_engine_types::events::Event) -> Result<Self, anyhow::Error> {
        Ok(Self {
            substate_id: event.substate_id().map(|sub_id| sub_id.to_string()),
            template_address: event.template_address().into_array(),
            tx_hash: event.tx_hash().into_array(),
            topic: event.topic(),
            payload: event.into_payload().into_iter().collect(),
        })
    }
}

pub(crate) type EventSchema = Schema<EventQuery, EmptyMutation, EmptySubscription>;

pub struct EventQuery;

#[Object]
impl EventQuery {
    pub async fn get_events_for_transaction(
        &self,
        ctx: &Context<'_>,
        tx_hash: String,
    ) -> Result<Vec<Event>, anyhow::Error> {
        info!(target: LOG_TARGET, "Querying events for transaction hash = {}", tx_hash);
        let event_manager = ctx.data_unchecked::<Arc<EventManager>>();
        let tx_id = TransactionId::from_hex(&tx_hash)?;
        let events = match event_manager.scan_events_for_transaction(tx_id).await {
            Ok(events) => events,
            Err(e) => {
                info!(
                    target: LOG_TARGET,
                    "Failed to scan events for transaction {} with error {}", tx_hash, e
                );
                return Err(e);
            },
        };

        let events = events
            .iter()
            .map(|e| Event::from_engine_event(e.clone()))
            .collect::<Result<Vec<Event>, _>>()?;

        Ok(events)
    }

    pub async fn get_events_for_substate(
        &self,
        ctx: &Context<'_>,
        substate_id: String,
        version: Option<u32>,
    ) -> Result<Vec<Event>, anyhow::Error> {
        let version = version.unwrap_or_default();
        info!(
            target: LOG_TARGET,
            "Querying events for substate_id = {}, starting from version = {}", substate_id, version
        );
        let event_manager = ctx.data_unchecked::<Arc<EventManager>>();
        let events = event_manager
            .scan_events_for_substate_from_network(SubstateId::from_str(&substate_id)?, Some(version))
            .await?
            .iter()
            .map(|e| Event::from_engine_event(e.clone()))
            .collect::<Result<Vec<Event>, anyhow::Error>>()?;

        Ok(events)
    }

    pub async fn get_events_by_payload(
        &self,
        ctx: &Context<'_>,
        payload_key: String,
        payload_value: String,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Event>, anyhow::Error> {
        info!(
            target: LOG_TARGET,
            "Querying events. payload_key: {}, payload_value: {}, offset: {}, limit: {}, ", payload_key, payload_value, offset, limit,
        );
        let event_manager = ctx.data_unchecked::<Arc<EventManager>>();
        let events = event_manager
            .scan_events_by_payload(payload_key, payload_value, offset, limit)
            .await?
            .iter()
            .map(|e| Event::from_engine_event(e.clone()))
            .collect::<Result<Vec<Event>, anyhow::Error>>()?;

        Ok(events)
    }

    pub async fn get_events(
        &self,
        ctx: &Context<'_>,
        topic: Option<String>,
        substate_id: Option<String>,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<Event>, anyhow::Error> {
        info!(
            target: LOG_TARGET,
            "Querying events. topic: {:?}, substate_id: {:?}, offset: {}, limit: {}, ", topic, substate_id, offset, limit,
        );
        let substate_id = substate_id.map(|str| SubstateId::from_str(&str)).transpose()?;
        let event_manager = ctx.data_unchecked::<Arc<EventManager>>();
        let events = event_manager
            .get_events_from_db(None, substate_id, offset, limit)
            .await?
            .iter()
            .map(|e| Event::from_engine_event(e.clone()))
            .collect::<Result<Vec<Event>, anyhow::Error>>()?;

        Ok(events)
    }

    pub async fn save_event(
        &self,
        ctx: &Context<'_>,
        substate_id: String,
        template_address: String,
        tx_hash: String,
        topic: String,
        payload: String,
        version: u64,
        timestamp: u64,
    ) -> Result<Event, anyhow::Error> {
        info!(
            target: LOG_TARGET,
            "Saving event for substate_id = {}, tx_hash = {} and topic = {}", substate_id, tx_hash, topic
        );

        let substate_id = SubstateId::from_str(&substate_id)?;
        let template_address = Hash::from_str(&template_address)?;
        let tx_hash = TransactionId::from_hex(&tx_hash)?;

        let payload = serde_json::from_str(&payload)?;
        let event_manager = ctx.data_unchecked::<Arc<EventManager>>();
        event_manager.save_event_to_db(
            &substate_id,
            template_address,
            tx_hash,
            topic.clone(),
            &payload,
            version,
            timestamp,
        )?;

        Ok(Event {
            substate_id: Some(substate_id.to_string()),
            template_address: template_address.into_array(),
            tx_hash: tx_hash.into_array(),
            topic,
            payload: payload.into_iter().collect(),
        })
    }
}
