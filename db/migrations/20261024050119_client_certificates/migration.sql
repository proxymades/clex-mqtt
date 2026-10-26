-- AlterTable
ALTER TABLE "mqtt_users" ADD COLUMN     "require_client_certificate" BOOLEAN NOT NULL DEFAULT false;

-- CreateTable
CREATE TABLE "mqtt_client_certificates" (
    "id" UUID NOT NULL,
    "mqtt_user_id" UUID NOT NULL,
    "fingerprint" BYTEA NOT NULL,
    "label" TEXT NOT NULL,
    "subject" TEXT NOT NULL,
    "expires_at" TIMESTAMPTZ(3) NOT NULL,
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "mqtt_client_certificates_pkey" PRIMARY KEY ("id")
);

-- CreateIndex
CREATE UNIQUE INDEX "mqtt_client_certificates_fingerprint_key" ON "mqtt_client_certificates"("fingerprint");

-- CreateIndex
CREATE INDEX "mqtt_client_certificates_mqtt_user_id_idx" ON "mqtt_client_certificates"("mqtt_user_id");

-- AddForeignKey
ALTER TABLE "mqtt_client_certificates" ADD CONSTRAINT "mqtt_client_certificates_mqtt_user_id_fkey" FOREIGN KEY ("mqtt_user_id") REFERENCES "mqtt_users"("id") ON DELETE CASCADE ON UPDATE CASCADE;
