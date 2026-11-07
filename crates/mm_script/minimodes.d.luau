--!strict
-- Type definitions for Minimodes game scripts (API v1). The web editor reads
-- this file for completions and hovers.
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

-- Only in match games. The platform runs the flow: it waits for MinPlayers,
-- counts down, starts (no one else may join), and after `End` shows the
-- result and sends everyone back to the lobby. In a draft the flow moves only
-- on `/match start` and `/match end`.
export type Match = {
	-- "Waiting", "Starting", "Started" or "Ended".
	State: string,
	-- Between 1 and MaxPlayers; 2 by default.
	MinPlayers: number,
	MaxPlayers: number,
	-- Up to 60; 10 by default.
	CountdownSeconds: number,
	-- Fires with the countdown's length in seconds.
	Starting: Signal<number>,
	Started: Signal<()>,
	Ended: Signal<{ Player }>,
	End: (self: Match, winners: (Player | { Player })?) -> (),
}

declare game: {
	GameId: number,
	-- The published version running, or nil in a draft.
	Version: number?,
	IsDraft: boolean,
	-- Persistent worlds: Unix time the world last unloaded (nil the first
	-- time), so scripts can catch up on progress made offline.
	LastShutdown: number?,
	-- Runs when the instance shuts down (it emptied, or the match ended),
	-- after its players have left. The instance waits up to 5 seconds for
	-- callbacks to return.
	BindToClose: (self: any, callback: () -> ()) -> (),
	Match: Match?,
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

-- Values are plain data: nil, booleans, numbers, strings, and tables of
-- those (arrays, or tables with string keys). Every `...Async` call yields
-- until the database answers, and raises an error if it fails, so wrap
-- important saves in `pcall`. A game may make 60 requests a minute plus 10
-- per player. Drafts have their own data, apart from the published game's.
export type DataStore = {
	Name: string,
	GetAsync: (self: DataStore, key: string) -> any,
	SetAsync: (self: DataStore, key: string, value: any) -> (),
	-- `transform` gets the current value (nil if unset) and returns the new
	-- one, or nil to leave it unchanged. If another server writes the key
	-- meanwhile, `transform` runs again on the newer value.
	UpdateAsync: (self: DataStore, key: string, transform: (any) -> any) -> any,
	-- Adds `delta` (default 1) to a whole number and returns the result.
	IncrementAsync: (self: DataStore, key: string, delta: number?) -> number,
	-- Returns the value that was removed.
	RemoveAsync: (self: DataStore, key: string) -> any,
}

export type DataStoreEntry = { key: string, value: number }

export type DataStorePages = {
	IsFinished: boolean,
	GetCurrentPage: (self: DataStorePages) -> { DataStoreEntry },
	AdvanceToNextPageAsync: (self: DataStorePages) -> (),
}

-- Ordered stores hold whole numbers and list them in order (leaderboards).
export type OrderedDataStore = {
	Name: string,
	GetAsync: (self: OrderedDataStore, key: string) -> number?,
	SetAsync: (self: OrderedDataStore, key: string, value: number) -> (),
	UpdateAsync: (self: OrderedDataStore, key: string, transform: (number?) -> number?) -> number?,
	IncrementAsync: (self: OrderedDataStore, key: string, delta: number?) -> number,
	RemoveAsync: (self: OrderedDataStore, key: string) -> number?,
	-- Sorted by value, then key. `pageSize` is 1-100 (default 50).
	GetSortedAsync: (
		self: OrderedDataStore,
		ascending: boolean,
		pageSize: number?,
		minValue: number?,
		maxValue: number?
	) -> DataStorePages,
}

declare DataStoreService: {
	-- Names and keys are 1 to 50 bytes.
	GetDataStore: (self: any, name: string) -> DataStore,
	GetOrderedDataStore: (self: any, name: string) -> OrderedDataStore,
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
