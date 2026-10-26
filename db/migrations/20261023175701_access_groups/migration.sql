-- AlterTable
ALTER TABLE "mqtt_acl_rules" ADD COLUMN     "group_id" UUID;

-- CreateTable
CREATE TABLE "mqtt_access_groups" (
    "id" UUID NOT NULL,
    "name" TEXT NOT NULL,
    "description" TEXT NOT NULL DEFAULT '',
    "enabled" BOOLEAN NOT NULL DEFAULT true,
    "created_at" TIMESTAMPTZ(3) NOT NULL DEFAULT CURRENT_TIMESTAMP,

    CONSTRAINT "mqtt_access_groups_pkey" PRIMARY KEY ("id")
);

-- CreateTable
CREATE TABLE "mqtt_user_groups" (
    "mqtt_user_id" UUID NOT NULL,
    "group_id" UUID NOT NULL,

    CONSTRAINT "mqtt_user_groups_pkey" PRIMARY KEY ("mqtt_user_id","group_id")
);

-- CreateIndex
CREATE UNIQUE INDEX "mqtt_access_groups_name_key" ON "mqtt_access_groups"("name");

-- CreateIndex
CREATE INDEX "mqtt_user_groups_group_id_idx" ON "mqtt_user_groups"("group_id");

-- CreateIndex
CREATE INDEX "mqtt_acl_rules_group_id_idx" ON "mqtt_acl_rules"("group_id");

-- AddForeignKey
ALTER TABLE "mqtt_acl_rules" ADD CONSTRAINT "mqtt_acl_rules_group_id_fkey" FOREIGN KEY ("group_id") REFERENCES "mqtt_access_groups"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "mqtt_user_groups" ADD CONSTRAINT "mqtt_user_groups_mqtt_user_id_fkey" FOREIGN KEY ("mqtt_user_id") REFERENCES "mqtt_users"("id") ON DELETE CASCADE ON UPDATE CASCADE;

-- AddForeignKey
ALTER TABLE "mqtt_user_groups" ADD CONSTRAINT "mqtt_user_groups_group_id_fkey" FOREIGN KEY ("group_id") REFERENCES "mqtt_access_groups"("id") ON DELETE CASCADE ON UPDATE CASCADE;
