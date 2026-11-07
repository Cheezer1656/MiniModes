--!strict
-- Type definitions for Minimodes game scripts (API v1, phase 5).
--
-- Scripts start at `main.luau`; other modules load with `require("path")`,
-- relative to the caller when the path starts with `./` or `../`, else from
-- the game's root. A module's body can't yield. Time is measured in ticks
-- (20 per second). Each tick a game gets a few milliseconds of CPU; a thread
-- that runs longer is paused and continues next tick, and a game that keeps
-- using its whole slice for 5 seconds is stopped.

export type Connection = {
	Connected: boolean,
	Disconnect: (self: Connection) -> (),
}

export type Signal<T...> = {
	Connect: (self: Signal<T...>, handler: (T...) -> ()) -> Connection,
	Once: (self: Signal<T...>, handler: (T...) -> ()) -> Connection,
	Wait: (self: Signal<T...>) -> T...,
}

export type Vector3 = {
	X: number,
	Y: number,
	Z: number,
	Magnitude: number,
	Unit: Vector3,
	Dot: (self: Vector3, other: Vector3) -> number,
	Cross: (self: Vector3, other: Vector3) -> Vector3,
	Lerp: (self: Vector3, goal: Vector3, alpha: number) -> Vector3,
	Floor: (self: Vector3) -> Vector3,
}

declare Vector3: {
	new: (x: number?, y: number?, z: number?) -> Vector3,
	zero: Vector3,
	one: Vector3,
}

export type Item = {
	Id: string,
	Count: number,
}

declare Item: {
	-- `id` is a Minecraft item id such as "diamond_sword".
	new: (id: string, props: { Name: string?, Lore: { string }?, Count: number? }?) -> Item,
}

export type GameMode = "survival" | "creative" | "adventure" | "spectator"

export type Player = {
	Name: string,
	UserId: string,
	-- Read-only snapshots, as of the start of this tick.
	Position: Vector3,
	Yaw: number,
	Pitch: number,
	HeldItem: string,
	-- Settable; the change shows up next tick.
	Health: number,
	Food: number,
	GameMode: GameMode,

	Chatted: Signal<string>,
	Died: Signal<>,
	Respawned: Signal<>,

	SendMessage: (self: Player, text: string) -> (),
	Title: (self: Player, title: string, subtitle: string?, fadeIn: number?, stay: number?, fadeOut: number?) -> (),
	ActionBar: (self: Player, text: string) -> (),
	Teleport: (self: Player, position: Vector3, yaw: number?, pitch: number?) -> (),
	Give: (self: Player, item: Item | string, count: number?) -> (),
	-- Slots 1-9 are the hotbar, 10-36 the main inventory; `nil` clears.
	SetSlot: (self: Player, slot: number, item: (Item | string)?) -> (),
	ClearInventory: (self: Player) -> (),
	Kill: (self: Player) -> (),
	PlaySound: (self: Player, sound: string, volume: number?, pitch: number?) -> (),
	SendToLobby: (self: Player) -> (),
	IsInGame: (self: Player) -> boolean,
}

-- Unset (nil) rules inherit: a region's from `game.Rules`, `game.Rules` from
-- the defaults, under which nothing may be broken or placed. Creative
-- players ignore rules.
export type Rules = {
	CanBreak: boolean?,
	CanPlace: boolean?,
	-- When set, only these blocks may be broken or placed.
	BreakableBlocks: { string }?,
	PlaceableBlocks: { string }?,
}

export type BlockEvent = {
	Player: Player?,
	Position: Vector3,
	-- The block broken, or the block placed.
	Block: string,
	PreviousBlock: string,
	-- Undoes the change (it already happened; the undo shows next tick).
	Cancel: (self: BlockEvent) -> (),
}

-- Block names are ids with optional properties: "stone", "oak_log[axis=x]".
-- A pattern maps blocks to weights: { stone = 70, coal_ore = 30 }.
export type Pattern = string | { [string]: number }

export type Region = {
	Min: Vector3,
	Max: Vector3,
	Rules: Rules,
	Entered: Signal<Player>,
	Left: Signal<Player>,
	Contains: (self: Region, position: Vector3) -> boolean,
	GetPlayers: (self: Region) -> { Player },
	Fill: (self: Region, pattern: Pattern) -> (),
	Refill: (self: Region, pattern: Pattern) -> (),
	Destroy: (self: Region) -> (),
}

declare Region: {
	-- Both corners are inclusive.
	new: (a: Vector3, b: Vector3) -> Region,
}

declare game: {
	Players: {
		PlayerAdded: Signal<Player>,
		PlayerRemoving: Signal<Player>,
		GetPlayers: (self: any) -> { Player },
		FindPlayer: (self: any, name: string) -> Player?,
	},
	World: {
		Min: Vector3,
		Max: Vector3,
		Spawn: Vector3,
		BlockBroken: Signal<BlockEvent>,
		BlockPlaced: Signal<BlockEvent>,
		GetBlock: ((self: any, position: Vector3) -> string) & ((self: any, x: number, y: number, z: number) -> string),
		-- Returns false outside the map.
		SetBlock: (self: any, position: Vector3, block: string) -> boolean,
		Fill: (self: any, a: Vector3, b: Vector3, block: string) -> (),
	},
	Rules: Rules,
}

declare task: {
	spawn: <A...>(f: ((A...) -> ()) | thread, A...) -> thread,
	defer: <A...>(f: ((A...) -> ()) | thread, A...) -> thread,
	delay: <A...>(seconds: number?, f: ((A...) -> ()) | thread, A...) -> thread,
	-- Returns the seconds actually waited (at least one tick).
	wait: (seconds: number?) -> number,
	cancel: (thread: thread) -> (),
}

-- Server time in seconds (ticks / 20); use differences, not the absolute value.
declare function time(): number
declare function warn(...: any): ()
