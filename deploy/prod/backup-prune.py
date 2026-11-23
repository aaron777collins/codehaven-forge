#!/usr/bin/env python3
"""Prune the backup archives in one directory.

Only regular files directly in DIR (never a symlink, never a directory) whose name matches
codehaven-<project>-YYYYMMDDTHHMMSSZ.tar.gz are considered; every other file is never touched
and never printed. The matching archives are sorted newest first by the date and time in
their name, not by mtime. Walking newest to oldest, the first archive seen for each calendar
date is kept until --keep-daily distinct dates have been kept, and the first archive seen for
each ISO week is kept until --keep-weekly distinct weeks have been kept; an archive may be
kept by both rules. Every matching archive that neither rule kept is printed, one file name
per line, sorted, and with --apply also deleted. Without --apply nothing is deleted: that is
the dry run.
"""

import argparse
import os
import re
import sys
from datetime import date


def die(message):
    print("backup-prune: %s" % message, file=sys.stderr)
    sys.exit(2)


def main():
    parser = argparse.ArgumentParser(description="Prune the backup archives in one directory.")
    parser.add_argument("--dir", required=True, help="the backup directory")
    parser.add_argument("--project", required=True, help="the project the archives belong to")
    parser.add_argument("--keep-daily", type=int, default=7, help="distinct days to keep (default 7)")
    parser.add_argument("--keep-weekly", type=int, default=4, help="distinct ISO weeks to keep (default 4)")
    parser.add_argument("--apply", action="store_true", help="delete the stale archives, not just print them")
    args = parser.parse_args()

    if args.keep_daily < 1:
        die("--keep-daily must be an integer of 1 or more")
    if args.keep_weekly < 1:
        die("--keep-weekly must be an integer of 1 or more")
    if not os.path.isdir(args.dir):
        die("the directory %s does not exist" % args.dir)

    name_re = re.compile(r"^codehaven-%s-(\d{8})T(\d{6})Z\.tar\.gz$" % re.escape(args.project))

    found = []
    with os.scandir(args.dir) as entries:
        for entry in entries:
            if not entry.is_file(follow_symlinks=False):
                continue
            match = name_re.match(entry.name)
            if match:
                found.append((match.group(1), match.group(2), entry.name))
    # Newest first: the date and time in the name decide, never the mtime.
    found.sort(reverse=True)

    keep = set()

    # The daily rule: the first archive of each of the newest keep-daily distinct dates.
    days = set()
    for day, _, name in found:
        if len(days) >= args.keep_daily:
            break
        if day not in days:
            days.add(day)
            keep.add(name)

    # The weekly rule: the same walk by ISO week. An archive may be kept by both rules.
    weeks = set()
    for day, _, name in found:
        if len(weeks) >= args.keep_weekly:
            break
        iso = date(int(day[0:4]), int(day[4:6]), int(day[6:8])).isocalendar()
        week = (iso[0], iso[1])
        if week not in weeks:
            weeks.add(week)
            keep.add(name)

    # What neither rule kept is stale: printed sorted, deleted only with --apply.
    for name in sorted(item[2] for item in found if item[2] not in keep):
        print(name)
        if args.apply:
            os.remove(os.path.join(args.dir, name))


if __name__ == "__main__":
    main()
