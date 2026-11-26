-- 全部看一遍时只取上次以后变了的文档（docs/affine-sync.md 7.11）：上次全部看一遍、这篇两边都没变时，AFFiNE 说它最后
-- 改的时间（同步协议的 space:load-doc-timestamps，毫秒，AFFiNE 服务端的钟，只拿来和 AFFiNE 下次说的比是不是一样）。
-- 存这篇同步的结果、停下、处理冲突时清掉；为空就照常取。
ALTER TABLE affine_docs ADD COLUMN affine_ts bigint;
