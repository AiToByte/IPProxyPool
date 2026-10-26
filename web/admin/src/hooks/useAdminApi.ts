import axios, { type AxiosInstance } from "axios";

const ADMIN_BASE = "/api/v1";

export interface NodeView {
  addr: string;
  ip: string;
  port: number;
  tier: string;
  country: string;
  provider: string;
  weight: number;
  proto: string;
  exit_ip: string | null;
}

export interface NodeFilter {
  tier?: string;
  country?: string;
  proto?: string;
}

export interface NodeWeightRequest {
  weight: number;
}

export interface AddNodeRequest {
  ip: string;
  port: number;
  country: string;
  tier: string;
  provider: string;
  weight: number;
  proto?: string;
  username?: string;
  password?: string;
}

export interface RemoveNodeResponse {
  addr: string;
  removed: boolean;
}

export interface NodeQuarantineRequest {
  ttl_secs: number;
}

export interface TenantSummary {
  tenant_id: string;
  api_key: string;
  is_active: boolean;
  qps: number;
  max_concurrency: number;
  in_flight: number;
  total_bytes: number;
  balance_usd: number;
}

export interface TenantDetail extends TenantSummary {}

export interface TenantBalanceRequest {
  balance_usd: number;
}

export interface TenantActiveRequest {
  active: boolean;
}

export interface VerifyStat {
  result: string;
  count: number;
}

export interface ProtoStat {
  proto: string;
  count: number;
}

export interface AnonymityStat {
  level: string;
  count: number;
}

export interface SuspendedStat {
  source: string;
  suspended: number;
}

export interface FreePoolSourceStat {
  source: string;
  yield_total: number;
  elite_total: number;
  fetch_ms_sum: number;
  fetch_count: number;
}

export interface FreePoolSnapshot {
  nodes_total: number;
  dry: boolean;
  verify: VerifyStat[];
  by_proto: ProtoStat[];
  sources: FreePoolSourceStat[];
  anonymity: AnonymityStat[];
  suspended: SuspendedStat[];
  intake_capped: number;
  baseline_fail: number;
  evicted: number;
}

export interface AutotuneStatus {
  enabled: boolean;
  boost_percent: number;
  dry: boolean;
}

export interface AutotuneRequest {
  enabled?: boolean;
  boost_percent?: number;
}

export interface BreakerTriggerStat {
  status: string;
  count: number;
}

export interface BreakerSnapshot {
  breaker_trigger: BreakerTriggerStat[];
  bandit_exploration: number;
  quarantine_domains: number;
  quarantine_nodes: number;
}

export interface HealthResponse {
  status: string;
  service: string;
}

export class AdminApiClient {
  private client: AxiosInstance;

  constructor(adminKey: string) {
    this.client = axios.create({
      baseURL: ADMIN_BASE,
      headers: {
        "X-Admin-Key": adminKey,
        "Content-Type": "application/json",
      },
    });
  }

  setAdminKey(key: string) {
    this.client.defaults.headers.common["X-Admin-Key"] = key;
  }

  async health(): Promise<HealthResponse> {
    const { data } = await this.client.get<HealthResponse>("/health");
    return data;
  }

  async listNodes(filter?: NodeFilter): Promise<NodeView[]> {
    const { data } = await this.client.get<NodeView[]>("/nodes", {
      params: filter,
    });
    return data;
  }

  async getNode(addr: string): Promise<NodeView> {
    const { data } = await this.client.get<NodeView>(`/nodes/${encodeURIComponent(addr)}`);
    return data;
  }

  async addNode(req: AddNodeRequest): Promise<NodeView> {
    const { data } = await this.client.post<NodeView>("/nodes", req);
    return data;
  }

  async deleteNode(addr: string): Promise<RemoveNodeResponse> {
    const { data } = await this.client.delete<RemoveNodeResponse>(
      `/nodes/${encodeURIComponent(addr)}`,
    );
    return data;
  }

  async patchNodeWeight(addr: string, weight: number): Promise<NodeView> {
    const { data } = await this.client.patch<NodeView>(
      `/nodes/${encodeURIComponent(addr)}/weight`,
      { weight },
    );
    return data;
  }

  async postNodeQuarantine(addr: string, ttl_secs: number): Promise<NodeView> {
    const { data } = await this.client.post<NodeView>(
      `/nodes/${encodeURIComponent(addr)}/quarantine`,
      { ttl_secs },
    );
    return data;
  }

  async deleteNodeQuarantine(addr: string): Promise<NodeView> {
    const { data } = await this.client.delete<NodeView>(
      `/nodes/${encodeURIComponent(addr)}/quarantine`,
    );
    return data;
  }

  async freePoolSummary(): Promise<FreePoolSnapshot> {
    const { data } = await this.client.get<FreePoolSnapshot>("/freepool/summary");
    return data;
  }

  async freePoolSources(): Promise<FreePoolSourceStat[]> {
    const { data } = await this.client.get<FreePoolSourceStat[]>("/freepool/sources");
    return data;
  }

  async autotuneStatus(): Promise<AutotuneStatus> {
    const { data } = await this.client.get<AutotuneStatus>("/freepool/autotune");
    return data;
  }

  async autotuneUpdate(req: AutotuneRequest): Promise<AutotuneStatus> {
    const { data } = await this.client.post<AutotuneStatus>("/freepool/autotune", req);
    return data;
  }

  async circuitSnapshot(): Promise<BreakerSnapshot> {
    const { data } = await this.client.get<BreakerSnapshot>("/circuit");
    return data;
  }

  async listTenants(): Promise<TenantSummary[]> {
    const { data } = await this.client.get<TenantSummary[]>("/tenants");
    return data;
  }

  async getTenant(apiKey: string): Promise<TenantDetail> {
    const { data } = await this.client.get<TenantDetail>(
      `/tenants/${encodeURIComponent(apiKey)}`,
    );
    return data;
  }

  async patchTenantBalance(apiKey: string, balance_usd: number): Promise<TenantDetail> {
    const { data } = await this.client.patch<TenantDetail>(
      `/tenants/${encodeURIComponent(apiKey)}/balance`,
      { balance_usd },
    );
    return data;
  }

  async patchTenantActive(apiKey: string, active: boolean): Promise<TenantDetail> {
    const { data } = await this.client.patch<TenantDetail>(
      `/tenants/${encodeURIComponent(apiKey)}/active`,
      { active },
    );
    return data;
  }
}
