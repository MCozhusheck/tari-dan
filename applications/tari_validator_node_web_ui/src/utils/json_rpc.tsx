//  Copyright 2022. The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

import type { GetFilteredBlocksCountRequest } from "@tari-project/typescript-bindings";
import type {
  VNAddPeerRequest,
  VNGetAllVnsRequest,
  VNGetAllVnsResponse,
  GetBlockRequest,
  GetBlockResponse,
  GetBlocksCountResponse,
  GetBlocksRequest,
  GetBlocksResponse,
  GetCommitteeRequest,
  GetCommitteeResponse,
  VNGetCommsStatsResponse,
  VNGetConnectionsResponse,
  GetEpochManagerStatsResponse,
  VNGetIdentityResponse,
  GetMempoolStatsResponse,
  GetNetworkCommitteeResponse,
  GetRecentTransactionsResponse,
  GetShardKeyRequest,
  GetShardKeyResponse,
  GetStateRequest,
  GetStateResponse,
  VNGetSubstateRequest,
  VNGetSubstateResponse,
  GetSubstatesByTransactionRequest,
  GetSubstatesByTransactionResponse,
  GetTemplateRequest,
  GetTemplateResponse,
  GetTemplatesRequest,
  GetTemplatesResponse,
  GetTransactionRequest,
  GetTransactionResponse,
  VNGetTransactionResultRequest,
  VNGetTransactionResultResponse,
  GetTxPoolResponse,
  ListBlocksRequest,
  ListBlocksResponse,
  VNSubmitTransactionRequest,
  VNSubmitTransactionResponse,
  VNGetValidatorFeesRequest,
  VNGetValidatorFeesResponse,
} from "@tari-project/typescript-bindings";


const DEFAULT_ADDRESS = new URL("http://127.0.0.1:18200");

export async function getClientAddress(): Promise<URL> {
  try {
    let resp = await fetch("/json_rpc_address");
    if (resp.status === 200) {
      let url = await resp.text();
      console.log("Got URL from server:", url);
      return new URL(`http://${url}`);
    }
  } catch (e) {
    console.warn(e);
  }

  return DEFAULT_ADDRESS;
}

async function jsonRpc(method: string, params: any = null) {
  let id = 0;
  id += 1;
  let address = await getClientAddress();
  let response = await fetch(address, {
    method: "POST",
    body: JSON.stringify({
      method: method,
      jsonrpc: "2.0",
      id,
      params: params,
    }),
    headers: {
      "Content-Type": "application/json",
    },
  });
  let json = await response.json();
  console.log(method, json);
  if (json.error) {
    throw json.error;
  }
  return json.result;
}

// Transaction
export const submitTransaction = (request: VNSubmitTransactionRequest): Promise<VNSubmitTransactionResponse> =>
  jsonRpc("submit_transaction", request);
export const getRecentTransactions = (): Promise<GetRecentTransactionsResponse> => jsonRpc("get_recent_transactions");
export const getTransaction = (request: GetTransactionRequest): Promise<GetTransactionResponse> =>
  jsonRpc("get_transaction", request);
export const getTransactionResult = (request: VNGetTransactionResultRequest): Promise<VNGetTransactionResultResponse> =>
  jsonRpc("get_transaction_result", request);
export const getState = (request: GetStateRequest): Promise<GetStateResponse> => jsonRpc("get_state", request);
export const getSubstate = (request: VNGetSubstateRequest): Promise<VNGetSubstateResponse> =>
  jsonRpc("get_substate", request);
export const getUpSubstates = (request: GetSubstatesByTransactionRequest): Promise<GetSubstatesByTransactionResponse> =>
  jsonRpc("get_substates_created_by_transaction", request);
export const getDownSubstates = (
  request: GetSubstatesByTransactionRequest,
): Promise<GetSubstatesByTransactionResponse> => jsonRpc("get_substates_destroyed_by_transaction", request);
export const listBlocks = (request: ListBlocksRequest): Promise<ListBlocksResponse> => jsonRpc("list_blocks", request);
export const getTxPool = (): Promise<GetTxPoolResponse> => jsonRpc("get_tx_pool");

// Blocks
export const getBlock = (request: GetBlockRequest): Promise<GetBlockResponse> => jsonRpc("get_block", request);
export const getBlocksCount = (): Promise<GetBlocksCountResponse> => jsonRpc("get_blocks_count");
export const getBlocks = (request: GetBlocksRequest): Promise<GetBlocksResponse> => jsonRpc("get_blocks", request);
export const getFilteredBlocksCount = (request: GetFilteredBlocksCountRequest): Promise<GetBlocksCountResponse> =>
  jsonRpc("get_filtered_blocks_count", request);

// Template
export const getTemplate = (request: GetTemplateRequest): Promise<GetTemplateResponse> =>
  jsonRpc("get_template", request);
export const getTemplates = (request: GetTemplatesRequest): Promise<GetTemplatesResponse> =>
  jsonRpc("get_templates", request);

// Validator Node
export const getIdentity = (): Promise<VNGetIdentityResponse> => jsonRpc("get_identity");

export const getMempoolStats = (): Promise<GetMempoolStatsResponse> => jsonRpc("get_mempool_stats");
export const getEpochManagerStats = (): Promise<GetEpochManagerStatsResponse> => jsonRpc("get_epoch_manager_stats");
export const getShardKey = (request: GetShardKeyRequest): Promise<GetShardKeyResponse> =>
  jsonRpc("get_shard_key", request);
export const getCommittee = (request: GetCommitteeRequest): Promise<GetCommitteeResponse> =>
  jsonRpc("get_committee", request);
export const getAllVns = (request: VNGetAllVnsRequest): Promise<VNGetAllVnsResponse> => jsonRpc("get_all_vns", request);
export const getNetworkCommittees = (): Promise<GetNetworkCommitteeResponse> => jsonRpc("get_network_committees", {});
export const getFees = (request: VNGetValidatorFeesRequest): Promise<VNGetValidatorFeesResponse> =>
  jsonRpc("get_fees", request);

// Comms
export const addPeer = (request: VNAddPeerRequest) => jsonRpc("add_peer", request);
export const getCommsStats = (): Promise<VNGetCommsStatsResponse> => jsonRpc("get_comms_stats");
export const getConnections = (): Promise<VNGetConnectionsResponse> => jsonRpc("get_connections");
