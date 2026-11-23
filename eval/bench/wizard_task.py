#!/usr/bin/env python3
"""wizard_task.py <seed> : build the H2 task text and the expected submit JSON for one seed.

Writes expected/<seed>.json (never copied into site/) and prints the task text on stdout.
Importable: get_task(seed) -> (task_text, expected_dict).
"""
import json
import os
import random
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
REGIONS = {
    "United States": ["California", "New York", "Texas"],
    "Canada": ["Ontario", "Quebec", "Alberta"],
    "Germany": ["Bavaria", "Berlin", "Hesse"],
    "Japan": ["Tokyo", "Osaka", "Hokkaido"],
}
FIRST = ["Mara", "Tomas", "Ines", "Keiko", "Omar", "Priya", "Hugo", "Elena", "Liam", "Nadia"]
LAST = ["Lindqvist", "Okafor", "Tanaka", "Haddad", "Duarte", "Volkov", "Murphy", "Rossi", "Aziz", "Weber"]
CITIES = ["Portland", "Lyon", "Osaka Bay", "Halifax", "Leipzig", "Austin", "Tucson", "Kyoto"]


def get_task(seed):
    rng = random.Random(int(seed))
    first, last = rng.choice(FIRST), rng.choice(LAST)
    name = f"{first} {last}"
    email = f"{first.lower()}.{last.lower()}{rng.randint(10, 99)}@example.com"
    area, mid, tail = rng.randint(200, 899), rng.randint(200, 899), rng.randint(1000, 9999)
    phone_digits = f"{area}{mid}{tail}"
    country = rng.choice(sorted(REGIONS))
    region = rng.choice(REGIONS[country])
    city = rng.choice(CITIES)
    promo = f"SAVE{rng.randint(1000, 9999)}"
    expected = {
        "name": name, "email": email, "phone": phone_digits, "country": country, "region": region,
        "shipping_city": city, "promo": promo, "subscribe": False, "agree": True,
    }
    task = (
        f"完成向导：姓名 {name}、邮箱 {email}、电话 {area}-{mid}-{tail}（页面要求 10 位数字，按校验提示改正）、"
        f"国家 {country}、地区 {region}、收货城市 {city}、促销码 {promo}；"
        "不要订阅促销；同意条款；提交后用 answer 命令报告确认码。"
    )
    return task, expected


def main():
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        raise SystemExit("usage: wizard_task.py <seed>")
    seed = sys.argv[1]
    task, expected = get_task(seed)
    os.makedirs(os.path.join(HERE, "expected"), exist_ok=True)
    with open(os.path.join(HERE, "expected", f"{seed}.json"), "w", encoding="utf-8") as f:
        json.dump({"seed": int(seed), **expected}, f, ensure_ascii=False, indent=1)
    print(task)


if __name__ == "__main__":
    main()
