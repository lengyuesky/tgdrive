-- 应用目录范围：JSON 路径数组；空数组表示全盘，保持既有行为。
ALTER TABLE apps ADD COLUMN scope_paths TEXT NOT NULL DEFAULT '[]';
