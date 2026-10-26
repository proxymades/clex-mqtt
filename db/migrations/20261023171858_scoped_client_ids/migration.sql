/*
  Warnings:

  - A unique constraint covering the columns `[principal,client_id]` on the table `sessions` will be added. If there are existing duplicate values, this will fail.

*/
-- DropIndex
DROP INDEX "sessions_client_id_key";

-- AlterTable
ALTER TABLE "messages" ADD COLUMN     "source_principal" TEXT;

-- CreateIndex
CREATE UNIQUE INDEX "sessions_principal_client_id_key" ON "sessions"("principal", "client_id");
