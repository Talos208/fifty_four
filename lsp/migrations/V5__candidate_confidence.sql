-- LLM が自己申告した候補ごとの確信度(0.0〜1.0)。NULL は未申告/不正値。
-- selected と突き合わせて、確信度と実際の採用率の相関を後から見るために記録する。
ALTER TABLE completion_candidates ADD COLUMN confidence REAL;
ALTER TABLE code_action_candidates ADD COLUMN confidence REAL;
