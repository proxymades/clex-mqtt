-- AlterTable
ALTER TABLE "deliveries" ADD COLUMN     "accounted_bytes" BIGINT;

-- AlterTable
ALTER TABLE "sessions" ADD COLUMN     "queued_bytes" BIGINT,
ADD COLUMN     "queued_count" BIGINT;
