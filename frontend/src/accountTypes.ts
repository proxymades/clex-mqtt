export interface AccountLimits {
  maxMqttUsers: number; maxConnections: number; maxSessions: number;
  maxQueuedMessages: number; maxQueuedBytes: number; maxPublishRate: number;
}
export interface AccountSession {
  id: string; username: string; csrfToken: string; topicPrefix: string; limits: AccountLimits;
  usage: { mqttUsers: number; connections: number; storedSessions: number; storedMessages: number; storedBytes: number };
  connection: { host: string; tlsPort: number | null; websocketPath: string };
}
export interface PersonalMqttUser {
  id: string; label: string; username: string; enabled: boolean; createdAt: string;
  publishFilters: string[]; subscribeFilters: string[];
}
export interface ManagedAccount {
  id: string; username: string; enabled: boolean; createdAt: string; topicPrefix: string;
  limits: AccountLimits; mqttUsers: number; connections: number;
}
