# Let's start by defining some common terms we use
 - Client: refers to the game that is sending commands to the LocalServer
 - LocalServer: refers to the instance of Proxa running on your machine to playback the voice chat
 - ExternalServer: refers to the server in which the LocalServer communicates to for voice chat data

# All Commands
 - All commands follow this structure
   - **id (u8)**: the command id
   - **data**: the command data, this is variable in size and based on where the command was recieved from

# Client -> LocalServer
<!-- Listener Related Events 0-9 -->
 - 0: Set Right-Handed Listener Matrix
   - **M11 (f32)**
   - **M21 (f32)**
   - **M31 (f32)**
   - **M41 (f32)**
   - **M12 (f32)**
   - **M22 (f32)**
   - **M32 (f32)**
   - **M42 (f32)**
   - **M13 (f32)**
   - **M23 (f32)**
   - **M33 (f32)**
   - **M43 (f32)**
   - **M14 (f32)**
   - **M24 (f32)**
   - **M34 (f32)**
   - **M44 (f32)**
 - 1: Set Left-Handed Listener Matrix
   - **M11 (f32)**
   - **M21 (f32)**
   - **M31 (f32)**
   - **M41 (f32)**
   - **M12 (f32)**
   - **M22 (f32)**
   - **M32 (f32)**
   - **M42 (f32)**
   - **M13 (f32)**
   - **M23 (f32)**
   - **M33 (f32)**
   - **M43 (f32)**
   - **M14 (f32)**
   - **M24 (f32)**
   - **M34 (f32)**
   - **M44 (f32)**
 - 2: Set Listener Position & Rotation (Degrees)
   - **PX (f32)**
   - **PY (f32)**
   - **PZ (f32)**
   - **RX (f32)**
   - **RY (f32)**
   - **RZ (f32)**
 - 5: Set Listener Rolloff Settings
	- **curveType (u8)**
	- **rolloffStart (f32)**
	- **rolloffEnd (f32)**
<!-- Speaker Related Events 10-19 -->
 - 10: Set Speaker Position
   - **speakerId (u16)**
   - **X (f32)**
   - **Y (f32)**
   - **Z (f32)**
 - 11: Set Speaker Flags
   - **speakerId (u16)**
   - **flagsBitmask (u8)**
     - **IsMuted**: Mutes the speaker making them inaudible, E.G the speaker is a spectator and you're still alive (Defaults to false)
	 - **IsGlobal**: Sets the speaker to completely ignore any audio panning, like a game lobby before you load in (Defaults to true if no position is set, however once position data is recieved if this was never changed we assume this to be false)
	 - **AutoVolumeRolloff**: (Defaults to true, if no `Set Listener Rolloff Settings`/`Set Speaker Rolloff Settings` event is passed we will assume defaults)
	 - **DoDopplerEffect**: (Defaults to false, we may not implement this but this bit is reserved for that)
	 - **Reserved**:
	 - **Reserved**:
	 - **Reserved**:
	 - **Reserved**:
   - **setFlagsBitmask (u8)**
     - same as previous bitmask, but set bits for flags you're actually changing
	 - this is a bit of a hack but it means we use 2 bytes per 8 flags instead of 2 bytes per 1 flag (a u8 of 0 or 1)
 - 12: Set Speaker Rolloff Settings
	same as `Set Listener Rolloff Settings` but we can override it per speaker
	- **speakerId (u16)**
	- **curveType (u8)**
	- **rolloffStart (f32)**
	- **rolloffEnd (f32)**
 - 19: Reset Speaker Data
	- unsets the `Speaker Position`, `Speaker Flags`, and `Rolloff Settings`
<!-- Audio Effects 20-39 -->
<!-- In-Game Settings Control (for optionally embedding Proxa's settings into the game) 30-49 -->
 - 30: Request Options
 - 40: Interact With Option
   - When this event is fired, an `Options Recieved` event is expected to be sent back with the updated UI state
   - you should prevent clicking before that state is recieved
   - additionally you should kick the user out of the Proxa settings if nothing is recieved back after 1 second (assume Proxa is dead)
   - **optionIndex (u16)**
   - **newValue (u16)**
<!-- Misc / Control 240-255 -->
 - 255: Logic End
   - This event is very critical to how Proxa works
   - You issue the proxy commands after all game logic is complete, then add this one to the end
   - This event then triggers `Speaker Volumes Updated` to be sent back, you can then read this before starting the logic loop back up, however if it isn't available don't wait for it, as this protocol is intended to operate by blindly sending UDP data to Proxa's port to recieve if it is running as well as being relatively light weight to make it easier to mod into any game, which unlike TCP doesn't signify it closing, you don't want to have closing Proxa result in the game hanging, additionally you can actually use recieving `Speaker Volumes Updated` as a sign that Proxa is running if you for example want to embed the Proxa settings into the game's option menu, obviously you'd not want that to be shown if Proxa isn't running

# LocalServer -> Client
<!-- Speaker Related Events 10-19 -->
 - 10: Speaker Volumes Updated
   - **totalSpeakers (u16)**
   - **volumes\[totalSpeakers\] (u8)**:
     - **speakerId (u16)**
     - **volume (u8)**: volume going from 0 to 255
<!-- In-Game Settings Control (for optionally embedding Proxa's settings into the game) 30-49 -->
 - 30: Options Recieved
   - Recieved whenever an interaction occurs or when the options are requested
   - this is essentially a very basic declarative UI for you to interpret to generate config menus
   - **totalOptions (u16)**
   - **option\[totalOptions\] (option)**
     - **type (optionType)**: (0: Toggle, 1: Dropdown, 2: Slider, 3: Button, 4: Label, 5: Tab)
	   - 0: Toggle
	     - This should show up as a toggle
		 - Set value to u16 max when true, and 0 when false
	     - **text (UTF8, u16 size)**
		 - **hover (UTF8, u16 size)**
		 - **value (u16)**: 0 or u16 max
	   - 1: Dropdown
	     - This should show up as a dropdown menu
		 - set value to the specified item index when selected
	     - **text (UTF8, u16 size)**
		 - **hover (UTF8, u16 size)**
		 - **selectedItem (u16)**
		 - **totalItems (u16)**
		 - **items\[totalItems\] (item)**
           - **text (UTF8, u16 size)**
	   - 2: Slider
	     - This should show up as a slider
	     - **text (UTF8, u16 size)**
		 - **hover (UTF8, u16 size)**
		 - **value (u16)**: the slider position from 0 to the max u16 value
	   - 3: Button
	     - This should show up as a clickable button
		 - set value to anything to signify a button click
	     - **text (UTF8, u16 size)**
		 - **hover (UTF8, u16 size)**
	   - 4: Label
	     - This should show up as a text label
	     - **text (UTF8, u16 size)**
		 - **hover (UTF8, u16 size)**
	   - 5: Tab
	     - This should show up as a list of tabs, it should combine with the previous tab if it comes after a previous tab definition
		 - set value to anything to signify a button click
	     - **text (UTF8, u16 size)**
		 - **hover (UTF8, u16 size)**
		 - **isActive (u8)**: whether this tab is currently the selected tab