-- AlterTable
ALTER TABLE "messages" ADD COLUMN     "compacted" BOOLEAN NOT NULL DEFAULT false,
ADD COLUMN     "owner_id" UUID;

-- CreateIndex
CREATE INDEX "messages_owner_id_compacted_idx" ON "messages"("owner_id", "compacted");

-- AddForeignKey
ALTER TABLE "messages" ADD CONSTRAINT "messages_owner_id_fkey" FOREIGN KEY ("owner_id") REFERENCES "accounts"("id") ON DELETE RESTRICT ON UPDATE CASCADE;
