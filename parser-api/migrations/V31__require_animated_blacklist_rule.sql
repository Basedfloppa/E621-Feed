-- The post card's "Block static images" entry used to be stored as `-animated`.
-- That was wrong twice over, and both mistakes pointed the same way: a blacklist
-- entry names what to HIDE, and the query builder stripped the leading `-` and
-- re-applied it as an exclusion — so the entry hid animated media, the opposite
-- of its label. The card now writes `require:animated` (see
-- `utils::blacklist`), which reaches e621 as a bare positive tag and is
-- re-checked in-process.
--
-- Rows written by the old card therefore still carry the inverted rule, and the
-- damage compounds: clicking the button again APPENDS `require:animated` next to
-- the stale `-animated`, and the two are contradictory — the same query both
-- requires and excludes `animated`, so e621 returns nothing and the feed stays
-- empty with no error to explain it.
--
-- Rewrite exactly those lines. Deliberately line-exact rather than a substring
-- `replace`: a `-animated` inside a longer token (`-animated_gif`,
-- `foo-animated`) or one sharing a line with other rules was not written by the
-- button, and rewriting it would invert a rule the user typed on purpose. A line
-- that is nothing but `-animated` has one plausible origin — the old button —
-- because `-animated` and plain `animated` mean the same thing to every parser
-- this codebase has ever had, so nothing else had a reason to write the dashed
-- spelling.
--
-- Everything else round-trips byte-for-byte: untouched lines are reassembled
-- unchanged and rejoined with the same `\n`, so only the matching lines differ.
-- A matched line is written out in canonical form, which drops any padding or CR
-- around it — the line is a single rule, and every reader of this column splits
-- on whitespace, so the padding carried no meaning to lose.
-- A user who had both spellings ends up with `require:animated` twice; duplicate
-- requirements are harmless (the query builder dedups and the matcher is a
-- predicate), and collapsing them would mean rewriting text this migration has
-- no business touching.

UPDATE accounts
   SET blacklisted_tags = (
           WITH RECURSIVE rebuild(rest, idx, acc, done) AS (
               SELECT blacklisted_tags, 0, '', 0
               UNION ALL
               SELECT CASE
                          WHEN instr(rest, char(10)) = 0 THEN ''
                          ELSE substr(rest, instr(rest, char(10)) + 1)
                      END,
                      idx + 1,
                      acc
                          || CASE WHEN idx = 0 THEN '' ELSE char(10) END
                          || CASE
                                 WHEN lower(trim(
                                          CASE
                                              WHEN instr(rest, char(10)) = 0 THEN rest
                                              ELSE substr(rest, 1, instr(rest, char(10)) - 1)
                                          END,
                                          char(9) || char(13) || ' '
                                      )) = '-animated'
                                 THEN 'require:animated'
                                 ELSE CASE
                                          WHEN instr(rest, char(10)) = 0 THEN rest
                                          ELSE substr(rest, 1, instr(rest, char(10)) - 1)
                                      END
                             END,
                      CASE WHEN instr(rest, char(10)) = 0 THEN 1 ELSE 0 END
                 FROM rebuild
                WHERE done = 0
           )
           SELECT acc FROM rebuild WHERE done = 1
       )
 WHERE blacklisted_tags LIKE '%-animated%';

UPDATE account_device_links
   SET blacklisted_tags = (
           WITH RECURSIVE rebuild(rest, idx, acc, done) AS (
               SELECT blacklisted_tags, 0, '', 0
               UNION ALL
               SELECT CASE
                          WHEN instr(rest, char(10)) = 0 THEN ''
                          ELSE substr(rest, instr(rest, char(10)) + 1)
                      END,
                      idx + 1,
                      acc
                          || CASE WHEN idx = 0 THEN '' ELSE char(10) END
                          || CASE
                                 WHEN lower(trim(
                                          CASE
                                              WHEN instr(rest, char(10)) = 0 THEN rest
                                              ELSE substr(rest, 1, instr(rest, char(10)) - 1)
                                          END,
                                          char(9) || char(13) || ' '
                                      )) = '-animated'
                                 THEN 'require:animated'
                                 ELSE CASE
                                          WHEN instr(rest, char(10)) = 0 THEN rest
                                          ELSE substr(rest, 1, instr(rest, char(10)) - 1)
                                      END
                             END,
                      CASE WHEN instr(rest, char(10)) = 0 THEN 1 ELSE 0 END
                 FROM rebuild
                WHERE done = 0
           )
           SELECT acc FROM rebuild WHERE done = 1
       )
 WHERE blacklisted_tags LIKE '%-animated%';
