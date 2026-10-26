-- AlterTable
ALTER TABLE "deliveries" ADD COLUMN     "shared_filter" TEXT;

-- AlterTable
ALTER TABLE "subscriptions" ADD COLUMN     "shared_last_sequence" BIGINT NOT NULL DEFAULT 0;

-- CreateIndex
CREATE INDEX "deliveries_shared_filter_idx" ON "deliveries"("shared_filter");
