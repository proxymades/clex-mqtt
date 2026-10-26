export interface Session {
  username: string;
  csrfToken: string;
}

export interface Listener {
  name: string;
  address: string;
  protocol: string;
  enabled: boolean;
}

export interface Capability {
  name: string;
  status: 'available' | 'partial' | 'planned';
  detail: string;
}

export interface Overview {
  version: string;
  uptimeSeconds: number;
  activeConnections: number;
  storedSessions: number;
  subscriptions: number;
  retainedMessages: number;
  queuedMessages: number;
  incomingMessages: number;
  outgoingMessages: number;
  rejectedConnections: number;
  rejectedPublications?: number;
  monitoring?: { enabled: boolean; path: string; databaseSnapshotSeconds: number };
  database: { healthy: boolean };
  listeners: Listener[];
  capabilities: Capability[];
}

export interface Client {
  certificateFingerprint?: string | null;
  sessionId: string;
  clientId: string;
  username: string;
  connected: boolean;
  connectedAt: string | null;
  lastSeen: string;
  subscriptions: number;
  queued: number;
  inflight: number;
  remoteAddress: string | null;
  protocol: string;
  keepAliveSeconds: number | null;
  expiresAt: string | null;
}

export interface RetainedMessage {
  topic: string;
  payloadBase64: string;
  payloadBytes: number;
  payloadTruncated: boolean;
  qos: number;
  updatedAt: string;
}

export interface BrokerUser {
  ownerId?: string | null;
  ownerUsername?: string | null;
  accountLabel?: string | null;
  accountEnabled?: boolean;
  requireClientCertificate?: boolean;
  certificateCount?: number;
  username: string;
  enabled: boolean;
  scramReady?: boolean;
  createdAt: string;
  publishFilters: string[];
  subscribeFilters: string[];
  groups: { id: string; name: string; enabled: boolean }[];
}

export interface AccessGroup {
  id: string;
  name: string;
  description: string;
  enabled: boolean;
  createdAt: string;
  memberCount: number;
  publishFilters: string[];
  subscribeFilters: string[];
  denyPublishFilters: string[];
  denySubscribeFilters: string[];
}

export interface AccessCheckResult {
  username: string;
  topic: string;
  action: 'publish' | 'subscribe' | 'receive';
  decision: {
    allowed: boolean;
    reason: string;
    matchingTotal: number;
    matchingRules: { id: string; filter: string; allow: boolean; source: 'personal' | 'group' | 'global'; groupId: string | null; groupName: string | null; coversRequest: boolean; overlapsRequest: boolean }[];
  };
}

export interface AuditEntry {
  ip?: string | null;
  result?: string | null;
  reason?: string | null;
  id: number | string;
  actor: string;
  action: string;
  target: string;
  createdAt: string;
}

export interface ClientCertificate {
  id: string; label: string; subject: string; fingerprint: string;
  expiresAt: string; enabled: boolean; createdAt: string;
}
export interface CertificateList {
  certificates: ClientCertificate[];
  requireClientCertificate: boolean;
  tls: { mode: 'none' | 'optional' | 'required'; address: string | null };
}

export interface MessageEvent {
  id: number;
  receivedAt: string;
  clientId: string;
  username: string;
  topic: string;
  qos: number;
  retain: boolean;
  will: boolean;
  payloadBase64: string;
  payloadBytes: number;
  payloadTruncated: boolean;
}

export interface MessagePage {
  instanceId: string;
  cursor: number;
  lost: number;
  hasMore: boolean;
  capacity: number;
  messages: MessageEvent[];
}

export interface BridgeRule { direction: 'out' | 'in'; filter: string; localPrefix: string; remotePrefix: string }
export interface Bridge {
  id: string; name: string; endpoint: string; username: string; hasPassword: boolean;
  caPem: string | null; rules: BridgeRule[]; enabled: boolean; revision: number;
  status: string; lastError: string | null; connectedAt: string | null;
  received: number; sent: number; queued: number; createdAt: string;
}
export interface BridgeList { bridges: Bridge[]; secretKeyReady: boolean; maxQueuedMessages: number; maxQueuedBytes: number }
export type PageId = 'overview' | 'clients' | 'messages' | 'bridges' | 'users' | 'accounts' | 'audit' | 'settings';
