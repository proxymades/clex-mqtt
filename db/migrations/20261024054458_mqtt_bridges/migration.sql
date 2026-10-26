-- CreateTable
CREATE TABLE "mqtt_bridges" (
    "id" UUID NOT NULL,
    "name" TEXT NOT NULL,
    "endpoint" TEXT NOT NULL,
    "username" TEXT NOT NULL DEFAULT '',
    "password" BYTEA,
    "ca_pem" TEXT,
    "rules" JSONB NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "revision" BIGINT NOT NULL DEFAULT 1,
    "remote_revision" BIGINT NOT NULL DEFAULT 0,
    "status" TEXT NOT NULL DEFAULT 'connecting',
    "last_error" TEXT,
    "connected_at" TIMESTAMPTZ(3),
    "received" BIGINT NOT NULL DEFAULT 0,
    "sent" BIGINT NOT NULL DEFAULT 0,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "mqtt_bridges_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "bridge_deliveries" (
    "id" UUID NOT NULL,
    "sequence" BIGSERIAL NOT NULL,
    "bridge_id" UUID NOT NULL,
    "message_id" UUID NOT NULL,
    "topic" BYTEA NOT NULL,

    CONSTRAINT "bridge_deliveries_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "mqtt_bridges_name_key" ON "mqtt_bridges"("name");

-- CreateIndex
CREATE UNIQUE INDEX "bridge_deliveries_sequence_key" ON "bridge_deliveries"("sequence");

-- CreateIndex
CREATE INDEX "bridge_deliveries_bridge_id_sequence_idx" ON "bridge_deliveries"("bridge_id", "sequence");

-- CreateIndex
CREATE INDEX "bridge_deliveries_message_id_idx" ON "bridge_deliveries"("message_id");

-- CreateIndex
CREATE UNIQUE INDEX "bridge_deliveries_bridge_id_message_id_topic_key" ON "bridge_deliveries"("bridge_id", "message_id", "topic");

-- AddForeignKey
ALTER TABLE "bridge_deliveries" ADD CONSTRAINT "bridge_deliveries_bridge_id_fkey" FOREIGN KEY ("bridge_id") REFERENCES "mqtt_bridges"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "bridge_deliveries" ADD CONSTRAINT "bridge_deliveries_message_id_fkey" FOREIGN KEY ("message_id") REFERENCES "messages"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
