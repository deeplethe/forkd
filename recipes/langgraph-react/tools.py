"""
Synthetic tools the demo agent can call. Pure functions, no external
deps, fully deterministic — so the demo is reproducible to the
extent the LLM is.

Naming intentionally generic so the agent has to pick the right
tool, not just one that's named for the task.
"""
from typing import Annotated


_WEATHER_DB = {
    "tokyo":     {"high_c": 24, "low_c": 18, "rain_mm": 2, "summary": "mild, light rain morning"},
    "kyoto":     {"high_c": 26, "low_c": 17, "rain_mm": 0, "summary": "sunny, dry"},
    "osaka":     {"high_c": 25, "low_c": 19, "rain_mm": 0, "summary": "sunny, warm"},
    "sapporo":   {"high_c": 15, "low_c":  9, "rain_mm": 0, "summary": "cool, partly cloudy"},
    "hakone":    {"high_c": 19, "low_c": 13, "rain_mm": 5, "summary": "cool, drizzle"},
    "nara":      {"high_c": 26, "low_c": 17, "rain_mm": 0, "summary": "sunny, dry"},
    "default":   {"high_c": 22, "low_c": 16, "rain_mm": 1, "summary": "moderate"},
}


_PLACES = {
    "tokyo": [
        ("Shibuya Crossing",   "landmark",  "free",      30),
        ("Tsukiji Outer Mkt",  "food",      "$$",        90),
        ("Senso-ji Temple",    "culture",   "free",      45),
        ("teamLab Planets",    "art",       "$$$",      120),
        ("Yanaka Old Town",    "neighborhood","free",     90),
    ],
    "kyoto": [
        ("Fushimi Inari",      "culture",   "free",     120),
        ("Nishiki Market",     "food",      "$$",        60),
        ("Arashiyama Bamboo",  "nature",    "free",      60),
        ("Pontocho Alley",     "neighborhood","$$",      90),
    ],
    "osaka": [
        ("Dotonbori",          "food",      "$$",       120),
        ("Osaka Castle",       "culture",   "$",         60),
        ("Kuromon Market",     "food",      "$$",        60),
    ],
}


def weather(city: Annotated[str, "City name, lowercase"]) -> dict:
    """Look up weather for a city. Returns highs, lows, rain.

    The agent calls this to decide if outdoor plans are wise.
    """
    return _WEATHER_DB.get(city.lower(), _WEATHER_DB["default"])


def search_places(
    city: Annotated[str, "City name, lowercase"],
    category: Annotated[str, "One of: any, food, culture, nature, art, neighborhood, landmark"] = "any",
) -> list:
    """List places to visit in `city`, optionally filtered by category.

    Returns up to 5 (name, category, price_band, minutes_needed) tuples.
    The agent uses these to compose an itinerary.
    """
    places = _PLACES.get(city.lower(), [])
    if category.lower() != "any":
        places = [p for p in places if p[1] == category.lower()]
    return [
        {"name": n, "category": c, "price": p, "minutes": m}
        for (n, c, p, m) in places[:5]
    ]


TOOLS_SPEC = [
    {
        "type": "function",
        "function": {
            "name": "weather",
            "description": "Look up weather for a city. Returns high temp °C, low temp °C, rain mm.",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name, lowercase"},
                },
                "required": ["city"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "search_places",
            "description": "List places to visit in a city. Returns up to 5 places with name, category, price band, and minutes needed.",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name, lowercase"},
                    "category": {
                        "type": "string",
                        "description": "Filter to one of: any, food, culture, nature, art, neighborhood, landmark",
                        "default": "any",
                    },
                },
                "required": ["city"],
            },
        },
    },
]


TOOL_FNS = {
    "weather": weather,
    "search_places": search_places,
}
