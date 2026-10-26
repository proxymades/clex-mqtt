-- CreateTable
CREATE TABLE "admin_users" (
    "id" UUID NOT NULL,
    "username" TEXT NOT NULL,
    "password_hash" TEXT NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "admin_users_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "admin_sessions" (
    "id" UUID NOT NULL,
    "admin_id" UUID NOT NULL,
    "token_hash" BYTEA NOT NULL,
    "csrf_hash" BYTEA NOT NULL,
    "expires_at" TIMESTAMPTZ(3) NOT NULL,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "admin_sessions_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "mqtt_users" (
    "id" UUID NOT NULL,
    "username" TEXT NOT NULL,
    "password_hash" TEXT NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "superuser" BOOLEAN NOT NULL DEFAULT false,
    "bound_client_id" BYTEA,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "mqtt_users_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "mqtt_acl_rules" (
    "id" UUID NOT NULL,
    "mqtt_user_id" UUID,
    "filter" TEXT NOT NULL,
    "publish" BOOLEAN NOT NULL DEFAULT false,
    "subscribe" BOOLEAN NOT NULL DEFAULT false,
    "allow" BOOLEAN NOT NULL DEFAULT true,
    "priority" INTEGER NOT NULL DEFAULT 0,

    CONSTRAINT "mqtt_acl_rules_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "sessions" (
    "id" UUID NOT NULL,
    "client_id" BYTEA NOT NULL,
    "principal" TEXT NOT NULL,
    "generation" BIGSERIAL NOT NULL,
    "protocol" INTEGER NOT NULL,
    "online" BOOLEAN NOT NULL DEFAULT false,
    "expiry_interval" BIGINT NOT NULL,
    "expires_at" TIMESTAMPTZ(3),
    "will_message_id" UUID,
    "will_delay_interval" BIGINT NOT NULL DEFAULT 0,
    "will_due_at" TIMESTAMPTZ(3),
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,
    "updated_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "sessions_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "subscriptions" (
    "id" UUID NOT NULL,
    "session_id" UUID NOT NULL,
    "filter" TEXT NOT NULL,
    "qos" INTEGER NOT NULL,
    "no_local" BOOLEAN NOT NULL DEFAULT false,
    "retain_as_published" BOOLEAN NOT NULL DEFAULT false,
    "retain_handling" INTEGER NOT NULL DEFAULT 0,
    "subscription_identifier" INTEGER,

    CONSTRAINT "subscriptions_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "messages" (
    "id" UUID NOT NULL,
    "topic" BYTEA NOT NULL,
    "payload" BYTEA NOT NULL,
    "qos" INTEGER NOT NULL,
    "retain" BOOLEAN NOT NULL,
    "properties" JSONB NOT NULL,
    "source_client_id" BYTEA,
    "expires_at" TIMESTAMPTZ(3),
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "messages_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "retained" (
    "topic" BYTEA NOT NULL,
    "message_id" UUID NOT NULL,

    CONSTRAINT "retained_pkey" PRIMARY KEY ("topic")
);

-- CreateTable
CREATE TABLE "deliveries" (
    "id" UUID NOT NULL,
    "sequence" BIGSERIAL NOT NULL,
    "session_id" UUID NOT NULL,
    "message_id" UUID NOT NULL,
    "qos" INTEGER NOT NULL,
    "retain" BOOLEAN NOT NULL,
    "packet_id" INTEGER,
    "phase" TEXT NOT NULL DEFAULT 'queued',
    "subscription_identifiers" JSONB NOT NULL DEFAULT '[]',

    CONSTRAINT "deliveries_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "incoming_qos2" (
    "session_id" UUID NOT NULL,
    "packet_id" INTEGER NOT NULL,
    "message_id" UUID,
    "released" BOOLEAN NOT NULL DEFAULT false,

    CONSTRAINT "incoming_qos2_pkey" PRIMARY KEY ("session_id","packet_id")
);

-- CreateTable
CREATE TABLE "audit_events" (
    "id" BIGSERIAL NOT NULL,
    "actor" TEXT NOT NULL,
    "action" TEXT NOT NULL,
    "subject" TEXT,
    "details" JSONB NOT NULL DEFAULT '{}',
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "audit_events_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "admin_users_username_key" ON "admin_users"("username");

-- CreateIndex
CREATE UNIQUE INDEX "admin_sessions_token_hash_key" ON "admin_sessions"("token_hash");

-- CreateIndex
CREATE INDEX "admin_sessions_expires_at_idx" ON "admin_sessions"("expires_at");

-- CreateIndex
CREATE UNIQUE INDEX "mqtt_users_username_key" ON "mqtt_users"("username");

-- CreateIndex
CREATE INDEX "mqtt_acl_rules_mqtt_user_id_priority_idx" ON "mqtt_acl_rules"("mqtt_user_id", "priority");

-- CreateIndex
CREATE UNIQUE INDEX "sessions_client_id_key" ON "sessions"("client_id");

-- CreateIndex
CREATE UNIQUE INDEX "sessions_generation_key" ON "sessions"("generation");

-- CreateIndex
CREATE INDEX "sessions_expires_at_idx" ON "sessions"("expires_at");

-- CreateIndex
CREATE INDEX "sessions_will_due_at_idx" ON "sessions"("will_due_at");

-- CreateIndex
CREATE UNIQUE INDEX "subscriptions_session_id_filter_key" ON "subscriptions"("session_id", "filter");

-- CreateIndex
CREATE INDEX "messages_expires_at_idx" ON "messages"("expires_at");

-- CreateIndex
CREATE INDEX "retained_message_id_idx" ON "retained"("message_id");

-- CreateIndex
CREATE UNIQUE INDEX "deliveries_sequence_key" ON "deliveries"("sequence");

-- CreateIndex
CREATE INDEX "deliveries_session_id_sequence_idx" ON "deliveries"("session_id", "sequence");

-- CreateIndex
CREATE INDEX "deliveries_message_id_idx" ON "deliveries"("message_id");

-- CreateIndex
CREATE UNIQUE INDEX "deliveries_session_id_packet_id_key" ON "deliveries"("session_id", "packet_id");

-- CreateIndex
CREATE INDEX "incoming_qos2_message_id_idx" ON "incoming_qos2"("message_id");

-- CreateIndex
CREATE INDEX "audit_events_created_at_idx" ON "audit_events"("created_at");

-- AddForeignKey
ALTER TABLE "admin_sessions" ADD CONSTRAINT "admin_sessions_admin_id_fkey" FOREIGN KEY ("admin_id") REFERENCES "admin_users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "mqtt_acl_rules" ADD CONSTRAINT "mqtt_acl_rules_mqtt_user_id_fkey" FOREIGN KEY ("mqtt_user_id") REFERENCES "mqtt_users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "sessions" ADD CONSTRAINT "sessions_will_message_id_fkey" FOREIGN KEY ("will_message_id") REFERENCES "messages"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "subscriptions" ADD CONSTRAINT "subscriptions_session_id_fkey" FOREIGN KEY ("session_id") REFERENCES "sessions"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "retained" ADD CONSTRAINT "retained_message_id_fkey" FOREIGN KEY ("message_id") REFERENCES "messages"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "deliveries" ADD CONSTRAINT "deliveries_session_id_fkey" FOREIGN KEY ("session_id") REFERENCES "sessions"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "deliveries" ADD CONSTRAINT "deliveries_message_id_fkey" FOREIGN KEY ("message_id") REFERENCES "messages"("id") ON DELETE RESTRICT ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "incoming_qos2" ADD CONSTRAINT "incoming_qos2_session_id_fkey" FOREIGN KEY ("session_id") REFERENCES "sessions"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "incoming_qos2" ADD CONSTRAINT "incoming_qos2_message_id_fkey" FOREIGN KEY ("message_id") REFERENCES "messages"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
