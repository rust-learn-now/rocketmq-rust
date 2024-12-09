/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use cheetah_string::CheetahString;
use rocketmq_common::common::broker::broker_config::BrokerConfig;
use rocketmq_common::common::message::message_decoder;
use rocketmq_common::common::message::message_ext_broker_inner::MessageExtBrokerInner;
use rocketmq_common::common::message::MessageConst;
use rocketmq_common::common::message::MessageTrait;
use rocketmq_common::common::pop_ack_constants::PopAckConstants;
use rocketmq_common::common::FAQUrl;
use rocketmq_common::TimeUtils::get_current_millis;
use rocketmq_remoting::code::request_code::RequestCode;
use rocketmq_remoting::code::response_code::ResponseCode;
use rocketmq_remoting::net::channel::Channel;
use rocketmq_remoting::protocol::header::change_invisible_time_request_header::ChangeInvisibleTimeRequestHeader;
use rocketmq_remoting::protocol::header::change_invisible_time_response_header::ChangeInvisibleTimeResponseHeader;
use rocketmq_remoting::protocol::header::extra_info_util::ExtraInfoUtil;
use rocketmq_remoting::protocol::remoting_command::RemotingCommand;
use rocketmq_remoting::protocol::RemotingSerializable;
use rocketmq_remoting::remoting_error::RemotingError::RemotingCommandError;
use rocketmq_remoting::runtime::connection_handler_context::ConnectionHandlerContext;
use rocketmq_rust::ArcMut;
use rocketmq_store::base::message_result::PutMessageResult;
use rocketmq_store::base::message_status_enum::PutMessageStatus;
use rocketmq_store::log_file::MessageStore;
use rocketmq_store::pop::ack_msg::AckMsg;
use rocketmq_store::stats::broker_stats_manager::BrokerStatsManager;
use tracing::error;
use tracing::info;

use crate::failover::escape_bridge::EscapeBridge;
use crate::offset::manager::consumer_offset_manager::ConsumerOffsetManager;
use crate::offset::manager::consumer_order_info_manager::ConsumerOrderInfoManager;
use crate::processor::pop_message_processor::PopMessageProcessor;
use crate::processor::processor_service::pop_buffer_merge_service::PopBufferMergeService;
use crate::topic::manager::topic_config_manager::TopicConfigManager;

pub struct ChangeInvisibleTimeProcessor<MS> {
    broker_config: Arc<BrokerConfig>,
    topic_config_manager: TopicConfigManager,
    message_store: ArcMut<MS>,
    consumer_offset_manager: Arc<ConsumerOffsetManager>,
    consumer_order_info_manager: Arc<ConsumerOrderInfoManager>,
    broker_stats_manager: Arc<BrokerStatsManager>,
    pop_buffer_merge_service: ArcMut<PopBufferMergeService>,
    escape_bridge: ArcMut<EscapeBridge<MS>>,
    revive_topic: CheetahString,
    store_host: SocketAddr,
}

impl<MS> ChangeInvisibleTimeProcessor<MS> {
    pub fn new(
        broker_config: Arc<BrokerConfig>,
        topic_config_manager: TopicConfigManager,
        message_store: ArcMut<MS>,
        consumer_offset_manager: Arc<ConsumerOffsetManager>,
        consumer_order_info_manager: Arc<ConsumerOrderInfoManager>,
        broker_stats_manager: Arc<BrokerStatsManager>,
        pop_buffer_merge_service: ArcMut<PopBufferMergeService>,
        escape_bridge: ArcMut<EscapeBridge<MS>>,
    ) -> Self {
        let revive_topic = PopAckConstants::build_cluster_revive_topic(
            broker_config.broker_identity.broker_cluster_name.as_str(),
        );
        let store_host = format!("{}:{}", broker_config.broker_ip1, broker_config.listen_port)
            .parse::<SocketAddr>()
            .unwrap();
        ChangeInvisibleTimeProcessor {
            broker_config,
            topic_config_manager,
            message_store,
            consumer_offset_manager,
            consumer_order_info_manager,
            broker_stats_manager,
            pop_buffer_merge_service,
            escape_bridge,
            revive_topic: CheetahString::from_string(revive_topic),
            store_host,
        }
    }
}

impl<MS> ChangeInvisibleTimeProcessor<MS>
where
    MS: MessageStore,
{
    pub async fn process_request(
        &mut self,
        channel: Channel,
        ctx: ConnectionHandlerContext,
        _request_code: RequestCode,
        request: RemotingCommand,
    ) -> crate::Result<Option<RemotingCommand>> {
        self.process_request_inner(channel, ctx, request, true)
            .await
    }

    pub async fn process_request_inner(
        &mut self,
        channel: Channel,
        _ctx: ConnectionHandlerContext,
        request: RemotingCommand,
        _broker_allow_suspend: bool,
    ) -> crate::Result<Option<RemotingCommand>> {
        let request_header = request
            .decode_command_custom_header::<ChangeInvisibleTimeRequestHeader>()
            .map_err(|e| RemotingCommandError(e.to_string()))?;
        let topic_config = self
            .topic_config_manager
            .select_topic_config(&request_header.topic);
        if topic_config.is_none() {
            error!(
                "The topic {} not exist, consumer: {} ",
                request_header.topic,
                channel.remote_address()
            );

            return Ok(Some(
                RemotingCommand::create_response_command_with_code_remark(
                    ResponseCode::TopicNotExist,
                    format!(
                        "topic[{}] not exist, apply first please! {}",
                        request_header.topic,
                        FAQUrl::suggest_todo(FAQUrl::APPLY_TOPIC_URL)
                    ),
                ),
            ));
        }
        let topic_config = topic_config.unwrap();
        if request_header.queue_id >= topic_config.read_queue_nums as i32
            || request_header.queue_id < 0
        {
            let error_info = format!(
                "queueId[{}] is illegal, topic:[{}] topicConfig.readQueueNums:[{}] consumer:[{}]",
                request_header.queue_id,
                request_header.topic,
                topic_config.read_queue_nums,
                channel.remote_address()
            );

            error!("{}", error_info);

            return Ok(Some(
                RemotingCommand::create_response_command_with_code_remark(
                    ResponseCode::MessageIllegal,
                    error_info,
                ),
            ));
        }
        let mix_offset = self
            .message_store
            .get_min_offset_in_queue(&request_header.topic, request_header.queue_id);
        let max_offset = self
            .message_store
            .get_max_offset_in_queue(&request_header.topic, request_header.queue_id);
        if request_header.offset < mix_offset || request_header.offset > max_offset {
            let info = format!(
                "request offset[{}] not in queue offset range[{}-{}], topic:[{}] consumer:[{}]",
                request_header.offset,
                mix_offset,
                max_offset,
                request_header.topic,
                channel.remote_address()
            );

            info!("{}", info);

            return Ok(Some(
                RemotingCommand::create_response_command_with_code_remark(
                    ResponseCode::NoMessage,
                    info,
                ),
            ));
        }
        let extra_info = ExtraInfoUtil::split(&request_header.extra_info)?;
        if ExtraInfoUtil::is_order(extra_info.as_slice()) {
            return self
                .process_change_invisible_time_for_order(&request_header, extra_info.as_slice())
                .await;
        }
        // add new ck
        let now = get_current_millis();
        let revive_qid = ExtraInfoUtil::get_revive_qid(extra_info.as_slice())?;
        let ck_result = self
            .append_check_point(
                &request_header,
                revive_qid,
                now,
                CheetahString::from_string(ExtraInfoUtil::get_broker_name(extra_info.as_slice())?),
            )
            .await;
        match ck_result.put_message_status() {
            PutMessageStatus::PutOk
            | PutMessageStatus::FlushDiskTimeout
            | PutMessageStatus::FlushSlaveTimeout
            | PutMessageStatus::SlaveNotAvailable => {}
            _ => {
                error!(
                    "change Invisible, put new ck error: {}",
                    ck_result.put_message_status()
                );
                return Ok(Some(
                    RemotingCommand::create_response_command_with_code_remark(
                        ResponseCode::SystemError,
                        format!(
                            "append check point error, status: {:?}",
                            ck_result.put_message_status()
                        ),
                    ),
                ));
            }
        }
        if let Err(e) = self
            .ack_origin(&request_header, extra_info.as_slice())
            .await
        {
            error!(
                "change Invisible, put ack msg error: {}, {}",
                request_header.extra_info, e
            );
        }
        let response_header = ChangeInvisibleTimeResponseHeader {
            pop_time: now,
            revive_qid,
            invisible_time: request_header.invisible_time,
        };
        Ok(Some(RemotingCommand::create_response_command_with_header(
            response_header,
        )))
    }

    async fn ack_origin(
        &mut self,
        request_header: &ChangeInvisibleTimeRequestHeader,
        extra_info: &[String],
    ) -> crate::Result<()> {
        let ack_msg = AckMsg {
            ack_offset: request_header.offset,
            start_offset: ExtraInfoUtil::get_ck_queue_offset(extra_info)?,
            consumer_group: request_header.consumer_group.clone(),
            topic: request_header.topic.clone(),
            queue_id: request_header.queue_id,
            pop_time: ExtraInfoUtil::get_pop_time(extra_info)?,
            broker_name: CheetahString::from_string(ExtraInfoUtil::get_broker_name(extra_info)?),
        };

        let rq_id = ExtraInfoUtil::get_revive_qid(extra_info)?;
        self.broker_stats_manager.inc_broker_ack_nums(1);
        self.broker_stats_manager.inc_group_ack_nums(
            request_header.consumer_group.as_str(),
            request_header.topic.as_str(),
            1,
        );
        if self.pop_buffer_merge_service.add_ack(rq_id, &ack_msg) {
            return Ok(());
        }
        let mut inner = MessageExtBrokerInner::default();
        inner.set_topic(self.revive_topic.clone());
        inner.set_body(Bytes::from(ack_msg.encode()?));
        inner.message_ext_inner.queue_id = rq_id;
        inner.set_tags(CheetahString::from_static_str(PopAckConstants::ACK_TAG));
        inner.message_ext_inner.born_timestamp = get_current_millis() as i64;
        inner.message_ext_inner.born_host = self.store_host;
        inner.message_ext_inner.store_host = self.store_host;
        let deliver_time_ms = ExtraInfoUtil::get_pop_time(extra_info)?
            + ExtraInfoUtil::get_invisible_time(extra_info)?;
        inner.set_delay_time_ms(deliver_time_ms as u64);
        inner.message_ext_inner.put_property(
            CheetahString::from_static_str(MessageConst::PROPERTY_UNIQ_CLIENT_MESSAGE_ID_KEYIDX),
            CheetahString::from(PopMessageProcessor::gen_ack_unique_id(&ack_msg)),
        );
        inner.properties_string =
            message_decoder::message_properties_to_string(inner.get_properties());
        let result = self
            .escape_bridge
            .put_message_to_specific_queue(inner)
            .await;
        match result.put_message_status() {
            PutMessageStatus::PutOk
            | PutMessageStatus::FlushDiskTimeout
            | PutMessageStatus::FlushSlaveTimeout
            | PutMessageStatus::SlaveNotAvailable
            | PutMessageStatus::ServiceNotAvailable => {}
            _ => {
                error!(
                    "change Invisible, put ack msg error: {}",
                    result.put_message_status()
                );
            }
        }
        //PopMetricsManager.incPopReviveAckPutCount(ackMsg,
        // putMessageResult.getPutMessageStatus());
        Ok(())
    }

    async fn append_check_point(
        &mut self,
        _request_header: &ChangeInvisibleTimeRequestHeader,
        _revive_qid: i32,
        _pop_time: u64,
        _broker_name: CheetahString,
    ) -> PutMessageResult {
        unimplemented!("ChangeInvisibleTimeProcessor append_check_point")
    }

    async fn process_change_invisible_time_for_order(
        &mut self,
        _request_header: &ChangeInvisibleTimeRequestHeader,
        _extra_info: &[String],
    ) -> crate::Result<Option<RemotingCommand>> {
        unimplemented!("ChangeInvisibleTimeProcessor process_change_invisible_time_for_order")
    }
}
