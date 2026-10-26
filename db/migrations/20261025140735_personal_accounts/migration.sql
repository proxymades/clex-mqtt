/*
  Warnings:

  - A unique constraint covering the columns `[owner_id,account_label]` on the table `mqtt_users` will be added. If there are existing duplicate values, this will fail.

*/
-- AlterTable
ALTER TABLE "mqtt_users" ADD COLUMN     "account_label" TEXT,
ADD COLUMN     "owner_id" UUID;

-- CreateTable
CREATE TABLE "accounts" (
    "id" UUID NOT NULL,
    "username" TEXT NOT NULL,
    "password_hash" TEXT NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "max_mqtt_users" INTEGER NOT NULL DEFAULT 10,
    "max_connections" INTEGER NOT NULL DEFAULT 10,
    "max_sessions" INTEGER NOT NULL DEFAULT 100,
    "max_queued_messages" INTEGER NOT NULL DEFAULT 1000,
    "max_queued_bytes" BIGINT NOT NULL DEFAULT 8388608,
    "max_publish_rate" INTEGER NOT NULL DEFAULT 20,
    "publish_window" BIGINT NOT NULL DEFAULT 0,
    "publish_count" INTEGER NOT NULL DEFAULT 0,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "accounts_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "account_sessions" (
    "id" UUID NOT NULL,
    "account_id" UUID NOT NULL,
    "token_hash" BYTEA NOT NULL,
    "csrf_hash" BYTEA NOT NULL,
    "expires_at" TIMESTAMPTZ(3) NOT NULL,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "account_sessions_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "accounts_username_key" ON "accounts"("username");

-- CreateIndex
CREATE UNIQUE INDEX "account_sessions_token_hash_key" ON "account_sessions"("token_hash");

-- CreateIndex
CREATE INDEX "account_sessions_account_id_created_at_idx" ON "account_sessions"("account_id", "created_at");

-- CreateIndex
CREATE INDEX "account_sessions_expires_at_idx" ON "account_sessions"("expires_at");

-- CreateIndex
CREATE INDEX "mqtt_users_owner_id_idx" ON "mqtt_users"("owner_id");

-- CreateIndex
CREATE UNIQUE INDEX "mqtt_users_owner_id_account_label_key" ON "mqtt_users"("owner_id", "account_label");

-- AddForeignKey
ALTER TABLE "account_sessions" ADD CONSTRAINT "account_sessions_account_id_fkey" FOREIGN KEY ("account_id") REFERENCES "accounts"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "mqtt_users" ADD CONSTRAINT "mqtt_users_owner_id_fkey" FOREIGN KEY ("owner_id") REFERENCES "accounts"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
