-- Wave A: the judge's brief-fulfillment verdict for the winning lane, stored
-- as JSON ({"state":"fulfilled|unfulfilled|unverifiable","evidence":"..."})
-- next to the plain verdict-card text in judge_verdict. Nullable: runs judged
-- before this column existed simply render without the fulfillment line.
ALTER TABLE runs ADD COLUMN judge_brief_fulfillment TEXT;
